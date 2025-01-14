// Copyright 2012-2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use rustc::dep_graph::DepGraph;
use rustc::front;
use rustc::front::map as hir_map;
use rustc_mir as mir;
use rustc::mir::mir_map::MirMap;
use rustc::session::{Session, CompileResult, compile_result_from_err_count};
use rustc::session::config::{self, Input, OutputFilenames, OutputType};
use rustc::session::search_paths::PathKind;
use rustc::lint;
use rustc::middle::{self, dependency_format, stability, ty, reachable};
use rustc::middle::privacy::AccessLevels;
use rustc::middle::ty::TyCtxt;
use rustc::util::common::time;
use rustc::util::nodemap::NodeSet;
use rustc_borrowck as borrowck;
use rustc_resolve as resolve;
use rustc_metadata::macro_import;
use rustc_metadata::creader::LocalCrateReader;
use rustc_metadata::cstore::CStore;
use rustc_trans::back::link;
use rustc_trans::back::write;
use rustc_trans::trans;
use rustc_typeck as typeck;
use rustc_privacy;
use rustc_plugin::registry::Registry;
use rustc_plugin as plugin;
use rustc_front::hir;
use rustc_front::lowering::{lower_crate, LoweringContext};
use rustc_passes::{no_asm, loops, consts, const_fn, rvalues, static_recursion};
use super::Compilation;

use serialize::json;

use std::collections::HashMap;
use std::env;
use std::ffi::{OsString, OsStr};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use syntax::ast::{self, NodeIdAssigner};
use syntax::attr::{self, AttrMetaMethods};
use syntax::diagnostics;
use syntax::fold::Folder;
use syntax::parse::{self, PResult, token};
use syntax::util::node_count::NodeCounter;
use syntax::visit;
use syntax;
use syntax_ext;

pub fn compile_input(sess: &Session,
                     cstore: &CStore,
                     cfg: ast::CrateConfig,
                     input: &Input,
                     outdir: &Option<PathBuf>,
                     output: &Option<PathBuf>,
                     addl_plugins: Option<Vec<String>>,
                     control: &CompileController) -> CompileResult {
    macro_rules! controller_entry_point {
        ($point: ident, $tsess: expr, $make_state: expr, $phase_result: expr) => {{
            let state = $make_state;
            let phase_result: &CompileResult = &$phase_result;
            if phase_result.is_ok() || control.$point.run_callback_on_error {
                (control.$point.callback)(state);
            }

            if control.$point.stop == Compilation::Stop {
                return compile_result_from_err_count($tsess.err_count());
            }
        }}
    }

    // We need nested scopes here, because the intermediate results can keep
    // large chunks of memory alive and we want to free them as soon as
    // possible to keep the peak memory usage low
    let (outputs, trans) = {
        let (outputs, expanded_crate, id) = {
            let krate = match phase_1_parse_input(sess, cfg, input) {
                Ok(krate) => krate,
                Err(mut parse_error) => {
                    parse_error.emit();
                    return Err(1);
                }
            };

            controller_entry_point!(after_parse,
                                    sess,
                                    CompileState::state_after_parse(input, sess, outdir, &krate),
                                    Ok(()));

            let outputs = build_output_filenames(input, outdir, output, &krate.attrs, sess);
            let id = link::find_crate_name(Some(sess), &krate.attrs, input);
            let expanded_crate = try!(phase_2_configure_and_expand(sess,
                                                                   &cstore,
                                                                   krate,
                                                                   &id[..],
                                                                   addl_plugins));

            (outputs, expanded_crate, id)
        };

        controller_entry_point!(after_expand,
                                sess,
                                CompileState::state_after_expand(input,
                                                                 sess,
                                                                 outdir,
                                                                 &expanded_crate,
                                                                 &id[..]),
                                Ok(()));

        let expanded_crate = assign_node_ids(sess, expanded_crate);
        // Lower ast -> hir.
        let lcx = LoweringContext::new(sess, Some(&expanded_crate));
        let dep_graph = DepGraph::new(sess.opts.build_dep_graph);
        let mut hir_forest = time(sess.time_passes(),
                                  "lowering ast -> hir",
                                  || hir_map::Forest::new(lower_crate(&lcx, &expanded_crate),
                                                          dep_graph));

        // Discard MTWT tables that aren't required past lowering to HIR.
        if !sess.opts.debugging_opts.keep_mtwt_tables &&
           !sess.opts.debugging_opts.save_analysis {
            syntax::ext::mtwt::clear_tables();
        }

        let arenas = ty::CtxtArenas::new();
        let hir_map = make_map(sess, &mut hir_forest);

        write_out_deps(sess, &outputs, &id);

        {
            let _ignore = hir_map.dep_graph.in_ignore();
            controller_entry_point!(after_write_deps,
                                    sess,
                                    CompileState::state_after_write_deps(input,
                                                                         sess,
                                                                         outdir,
                                                                         &hir_map,
                                                                         &expanded_crate,
                                                                         &hir_map.krate(),
                                                                         &id[..],
                                                                         &lcx),
                                    Ok(()));
        }

        time(sess.time_passes(), "attribute checking", || {
            front::check_attr::check_crate(sess, &expanded_crate);
        });

        time(sess.time_passes(),
             "early lint checks",
             || lint::check_ast_crate(sess, &expanded_crate));

        let opt_crate = if sess.opts.debugging_opts.keep_ast ||
                           sess.opts.debugging_opts.save_analysis {
            Some(&expanded_crate)
        } else {
            drop(expanded_crate);
            None
        };

        try!(try!(phase_3_run_analysis_passes(sess,
                                              &cstore,
                                              hir_map,
                                              &arenas,
                                              &id,
                                              control.make_glob_map,
                                              |tcx, mir_map, analysis, result| {
            {
                // Eventually, we will want to track plugins.
                let _ignore = tcx.dep_graph.in_ignore();

                let state = CompileState::state_after_analysis(input,
                                                               &tcx.sess,
                                                               outdir,
                                                               opt_crate,
                                                               tcx.map.krate(),
                                                               &analysis,
                                                               mir_map.as_ref(),
                                                               tcx,
                                                               &lcx,
                                                               &id);
                (control.after_analysis.callback)(state);

                if control.after_analysis.stop == Compilation::Stop {
                    return Err(0usize);
                }
            }

            try!(result);

            if log_enabled!(::log::INFO) {
                println!("Pre-trans");
                tcx.print_debug_stats();
            }
            let trans = phase_4_translate_to_llvm(tcx,
                                                  mir_map.unwrap(),
                                                  analysis);

            if log_enabled!(::log::INFO) {
                println!("Post-trans");
                tcx.print_debug_stats();
            }

            // Discard interned strings as they are no longer required.
            token::get_ident_interner().clear();

            Ok((outputs, trans))
        })))
    };

    let phase5_result = phase_5_run_llvm_passes(sess, &trans, &outputs);

    controller_entry_point!(after_llvm,
                            sess,
                            CompileState::state_after_llvm(input, sess, outdir, &trans),
                            phase5_result);
    try!(phase5_result);

    phase_6_link_output(sess, &trans, &outputs);

    Ok(())
}


/// The name used for source code that doesn't originate in a file
/// (e.g. source from stdin or a string)
pub fn anon_src() -> String {
    "<anon>".to_string()
}

pub fn source_name(input: &Input) -> String {
    match *input {
        // FIXME (#9639): This needs to handle non-utf8 paths
        Input::File(ref ifile) => ifile.to_str().unwrap().to_string(),
        Input::Str(_) => anon_src(),
    }
}

/// CompileController is used to customise compilation, it allows compilation to
/// be stopped and/or to call arbitrary code at various points in compilation.
/// It also allows for various flags to be set to influence what information gets
/// collected during compilation.
///
/// This is a somewhat higher level controller than a Session - the Session
/// controls what happens in each phase, whereas the CompileController controls
/// whether a phase is run at all and whether other code (from outside the
/// the compiler) is run between phases.
///
/// Note that if compilation is set to stop and a callback is provided for a
/// given entry point, the callback is called before compilation is stopped.
///
/// Expect more entry points to be added in the future.
pub struct CompileController<'a> {
    pub after_parse: PhaseController<'a>,
    pub after_expand: PhaseController<'a>,
    pub after_write_deps: PhaseController<'a>,
    pub after_analysis: PhaseController<'a>,
    pub after_llvm: PhaseController<'a>,

    pub make_glob_map: resolve::MakeGlobMap,
}

impl<'a> CompileController<'a> {
    pub fn basic() -> CompileController<'a> {
        CompileController {
            after_parse: PhaseController::basic(),
            after_expand: PhaseController::basic(),
            after_write_deps: PhaseController::basic(),
            after_analysis: PhaseController::basic(),
            after_llvm: PhaseController::basic(),
            make_glob_map: resolve::MakeGlobMap::No,
        }
    }
}

pub struct PhaseController<'a> {
    pub stop: Compilation,
    // If true then the compiler will try to run the callback even if the phase
    // ends with an error. Note that this is not always possible.
    pub run_callback_on_error: bool,
    pub callback: Box<Fn(CompileState) -> () + 'a>,
}

impl<'a> PhaseController<'a> {
    pub fn basic() -> PhaseController<'a> {
        PhaseController {
            stop: Compilation::Continue,
            run_callback_on_error: false,
            callback: box |_| {},
        }
    }
}

/// State that is passed to a callback. What state is available depends on when
/// during compilation the callback is made. See the various constructor methods
/// (`state_*`) in the impl to see which data is provided for any given entry point.
pub struct CompileState<'a, 'ast: 'a, 'tcx: 'a> {
    pub input: &'a Input,
    pub session: &'a Session,
    pub cfg: Option<&'a ast::CrateConfig>,
    pub krate: Option<&'a ast::Crate>,
    pub crate_name: Option<&'a str>,
    pub output_filenames: Option<&'a OutputFilenames>,
    pub out_dir: Option<&'a Path>,
    pub expanded_crate: Option<&'a ast::Crate>,
    pub hir_crate: Option<&'a hir::Crate>,
    pub ast_map: Option<&'a hir_map::Map<'ast>>,
    pub mir_map: Option<&'a MirMap<'tcx>>,
    pub analysis: Option<&'a ty::CrateAnalysis<'a>>,
    pub tcx: Option<&'a TyCtxt<'tcx>>,
    pub lcx: Option<&'a LoweringContext<'a>>,
    pub trans: Option<&'a trans::CrateTranslation>,
}

impl<'a, 'ast, 'tcx> CompileState<'a, 'ast, 'tcx> {
    fn empty(input: &'a Input,
             session: &'a Session,
             out_dir: &'a Option<PathBuf>)
             -> CompileState<'a, 'ast, 'tcx> {
        CompileState {
            input: input,
            session: session,
            out_dir: out_dir.as_ref().map(|s| &**s),
            cfg: None,
            krate: None,
            crate_name: None,
            output_filenames: None,
            expanded_crate: None,
            hir_crate: None,
            ast_map: None,
            analysis: None,
            mir_map: None,
            tcx: None,
            lcx: None,
            trans: None,
        }
    }

    fn state_after_parse(input: &'a Input,
                         session: &'a Session,
                         out_dir: &'a Option<PathBuf>,
                         krate: &'a ast::Crate)
                         -> CompileState<'a, 'ast, 'tcx> {
        CompileState { krate: Some(krate), ..CompileState::empty(input, session, out_dir) }
    }

    fn state_after_expand(input: &'a Input,
                          session: &'a Session,
                          out_dir: &'a Option<PathBuf>,
                          expanded_crate: &'a ast::Crate,
                          crate_name: &'a str)
                          -> CompileState<'a, 'ast, 'tcx> {
        CompileState {
            crate_name: Some(crate_name),
            expanded_crate: Some(expanded_crate),
            ..CompileState::empty(input, session, out_dir)
        }
    }

    fn state_after_write_deps(input: &'a Input,
                              session: &'a Session,
                              out_dir: &'a Option<PathBuf>,
                              hir_map: &'a hir_map::Map<'ast>,
                              krate: &'a ast::Crate,
                              hir_crate: &'a hir::Crate,
                              crate_name: &'a str,
                              lcx: &'a LoweringContext<'a>)
                              -> CompileState<'a, 'ast, 'tcx> {
        CompileState {
            crate_name: Some(crate_name),
            ast_map: Some(hir_map),
            krate: Some(krate),
            hir_crate: Some(hir_crate),
            lcx: Some(lcx),
            ..CompileState::empty(input, session, out_dir)
        }
    }

    fn state_after_analysis(input: &'a Input,
                            session: &'a Session,
                            out_dir: &'a Option<PathBuf>,
                            krate: Option<&'a ast::Crate>,
                            hir_crate: &'a hir::Crate,
                            analysis: &'a ty::CrateAnalysis,
                            mir_map: Option<&'a MirMap<'tcx>>,
                            tcx: &'a TyCtxt<'tcx>,
                            lcx: &'a LoweringContext<'a>,
                            crate_name: &'a str)
                            -> CompileState<'a, 'ast, 'tcx> {
        CompileState {
            analysis: Some(analysis),
            mir_map: mir_map,
            tcx: Some(tcx),
            krate: krate,
            hir_crate: Some(hir_crate),
            lcx: Some(lcx),
            crate_name: Some(crate_name),
            ..CompileState::empty(input, session, out_dir)
        }
    }


    fn state_after_llvm(input: &'a Input,
                        session: &'a Session,
                        out_dir: &'a Option<PathBuf>,
                        trans: &'a trans::CrateTranslation)
                        -> CompileState<'a, 'ast, 'tcx> {
        CompileState { trans: Some(trans), ..CompileState::empty(input, session, out_dir) }
    }
}

pub fn phase_1_parse_input<'a>(sess: &'a Session,
                               cfg: ast::CrateConfig,
                               input: &Input)
                               -> PResult<'a, ast::Crate> {
    // These may be left in an incoherent state after a previous compile.
    // `clear_tables` and `get_ident_interner().clear()` can be used to free
    // memory, but they do not restore the initial state.
    syntax::ext::mtwt::reset_tables();
    token::reset_ident_interner();

    let krate = try!(time(sess.time_passes(), "parsing", || {
        match *input {
            Input::File(ref file) => {
                parse::parse_crate_from_file(file, cfg.clone(), &sess.parse_sess)
            }
            Input::Str(ref src) => {
                parse::parse_crate_from_source_str(anon_src().to_string(),
                                                   src.to_string(),
                                                   cfg.clone(),
                                                   &sess.parse_sess)
            }
        }
    }));

    if sess.opts.debugging_opts.ast_json_noexpand {
        println!("{}", json::as_json(&krate));
    }

    if sess.opts.debugging_opts.input_stats {
        println!("Lines of code:             {}", sess.codemap().count_lines());
        println!("Pre-expansion node count:  {}", count_nodes(&krate));
    }

    if let Some(ref s) = sess.opts.debugging_opts.show_span {
        syntax::show_span::run(sess.diagnostic(), s, &krate);
    }

    Ok(krate)
}

fn count_nodes(krate: &ast::Crate) -> usize {
    let mut counter = NodeCounter::new();
    visit::walk_crate(&mut counter, krate);
    counter.count
}

// For continuing compilation after a parsed crate has been
// modified

/// Run the "early phases" of the compiler: initial `cfg` processing,
/// loading compiler plugins (including those from `addl_plugins`),
/// syntax expansion, secondary `cfg` expansion, synthesis of a test
/// harness if one is to be provided and injection of a dependency on the
/// standard library and prelude.
///
/// Returns `None` if we're aborting after handling -W help.
pub fn phase_2_configure_and_expand(sess: &Session,
                                    cstore: &CStore,
                                    mut krate: ast::Crate,
                                    crate_name: &str,
                                    addl_plugins: Option<Vec<String>>)
                                    -> Result<ast::Crate, usize> {
    let time_passes = sess.time_passes();

    // strip before anything else because crate metadata may use #[cfg_attr]
    // and so macros can depend on configuration variables, such as
    //
    //   #[macro_use] #[cfg(foo)]
    //   mod bar { macro_rules! baz!(() => {{}}) }
    //
    // baz! should not use this definition unless foo is enabled.

    let mut feature_gated_cfgs = vec![];
    krate = try!(time(time_passes, "configuration 1", || {
        sess.track_errors(|| {
            syntax::config::strip_unconfigured_items(sess.diagnostic(),
                                                     krate,
                                                     &mut feature_gated_cfgs)
        })
    }));

    *sess.crate_types.borrow_mut() = collect_crate_types(sess, &krate.attrs);
    *sess.crate_metadata.borrow_mut() = collect_crate_metadata(sess, &krate.attrs);

    time(time_passes, "recursion limit", || {
        middle::recursion_limit::update_recursion_limit(sess, &krate);
    });

    try!(time(time_passes, "gated macro checking", || {
        sess.track_errors(|| {
            let features =
              syntax::feature_gate::check_crate_macros(sess.codemap(),
                                                       &sess.parse_sess.span_diagnostic,
                                                       &krate);

            // these need to be set "early" so that expansion sees `quote` if enabled.
            *sess.features.borrow_mut() = features;
        })
    }));


    krate = time(time_passes, "crate injection", || {
        syntax::std_inject::maybe_inject_crates_ref(krate, sess.opts.alt_std_name.clone())
    });

    let macros = time(time_passes,
                      "macro loading",
                      || macro_import::read_macro_defs(sess, &cstore, &krate));

    let mut addl_plugins = Some(addl_plugins);
    let registrars = time(time_passes, "plugin loading", || {
        plugin::load::load_plugins(sess, &cstore, &krate, addl_plugins.take().unwrap())
    });

    let mut registry = Registry::new(sess, &krate);

    time(time_passes, "plugin registration", || {
        if sess.features.borrow().rustc_diagnostic_macros {
            registry.register_macro("__diagnostic_used",
                                    diagnostics::plugin::expand_diagnostic_used);
            registry.register_macro("__register_diagnostic",
                                    diagnostics::plugin::expand_register_diagnostic);
            registry.register_macro("__build_diagnostic_array",
                                    diagnostics::plugin::expand_build_diagnostic_array);
        }

        for registrar in registrars {
            registry.args_hidden = Some(registrar.args);
            (registrar.fun)(&mut registry);
        }
    });

    let Registry { syntax_exts, early_lint_passes, late_lint_passes, lint_groups,
                   llvm_passes, attributes, mir_passes, .. } = registry;

    try!(sess.track_errors(|| {
        let mut ls = sess.lint_store.borrow_mut();
        for pass in early_lint_passes {
            ls.register_early_pass(Some(sess), true, pass);
        }
        for pass in late_lint_passes {
            ls.register_late_pass(Some(sess), true, pass);
        }

        for (name, to) in lint_groups {
            ls.register_group(Some(sess), true, name, to);
        }

        *sess.plugin_llvm_passes.borrow_mut() = llvm_passes;
        sess.mir_passes.borrow_mut().extend(mir_passes);
        *sess.plugin_attributes.borrow_mut() = attributes.clone();
    }));

    // Lint plugins are registered; now we can process command line flags.
    if sess.opts.describe_lints {
        super::describe_lints(&sess.lint_store.borrow(), true);
        return Err(0);
    }
    try!(sess.track_errors(|| sess.lint_store.borrow_mut().process_command_line(sess)));

    krate = time(time_passes, "expansion", || {
        // Windows dlls do not have rpaths, so they don't know how to find their
        // dependencies. It's up to us to tell the system where to find all the
        // dependent dlls. Note that this uses cfg!(windows) as opposed to
        // targ_cfg because syntax extensions are always loaded for the host
        // compiler, not for the target.
        let mut _old_path = OsString::new();
        if cfg!(windows) {
            _old_path = env::var_os("PATH").unwrap_or(_old_path);
            let mut new_path = sess.host_filesearch(PathKind::All)
                                   .get_dylib_search_paths();
            new_path.extend(env::split_paths(&_old_path));
            env::set_var("PATH", &env::join_paths(new_path).unwrap());
        }
        let features = sess.features.borrow();
        let cfg = syntax::ext::expand::ExpansionConfig {
            crate_name: crate_name.to_string(),
            features: Some(&features),
            recursion_limit: sess.recursion_limit.get(),
            trace_mac: sess.opts.debugging_opts.trace_macros,
        };
        let mut ecx = syntax::ext::base::ExtCtxt::new(&sess.parse_sess,
                                                      krate.config.clone(),
                                                      cfg,
                                                      &mut feature_gated_cfgs);
        syntax_ext::register_builtins(&mut ecx.syntax_env);
        let (ret, macro_names) = syntax::ext::expand::expand_crate(ecx,
                                                                   macros,
                                                                   syntax_exts,
                                                                   krate);
        if cfg!(windows) {
            env::set_var("PATH", &_old_path);
        }
        *sess.available_macros.borrow_mut() = macro_names;
        ret
    });

    // Needs to go *after* expansion to be able to check the results
    // of macro expansion.  This runs before #[cfg] to try to catch as
    // much as possible (e.g. help the programmer avoid platform
    // specific differences)
    try!(time(time_passes, "complete gated feature checking 1", || {
        sess.track_errors(|| {
            let features = syntax::feature_gate::check_crate(sess.codemap(),
                                                             &sess.parse_sess.span_diagnostic,
                                                             &krate,
                                                             &attributes,
                                                             sess.opts.unstable_features);
            *sess.features.borrow_mut() = features;
        })
    }));

    // JBC: make CFG processing part of expansion to avoid this problem:

    // strip again, in case expansion added anything with a #[cfg].
    krate = try!(sess.track_errors(|| {
        let krate = time(time_passes, "configuration 2", || {
            syntax::config::strip_unconfigured_items(sess.diagnostic(),
                                                     krate,
                                                     &mut feature_gated_cfgs)
        });

        time(time_passes, "gated configuration checking", || {
            let features = sess.features.borrow();
            feature_gated_cfgs.sort();
            feature_gated_cfgs.dedup();
            for cfg in &feature_gated_cfgs {
                cfg.check_and_emit(sess.diagnostic(), &features, sess.codemap());
            }
        });

        krate
    }));

    krate = time(time_passes, "maybe building test harness", || {
        syntax::test::modify_for_testing(&sess.parse_sess, &sess.opts.cfg, krate, sess.diagnostic())
    });

    krate = time(time_passes,
                 "prelude injection",
                 || syntax::std_inject::maybe_inject_prelude(&sess.parse_sess, krate));

    time(time_passes,
         "checking that all macro invocations are gone",
         || syntax::ext::expand::check_for_macros(&sess.parse_sess, &krate));

    time(time_passes,
         "checking for inline asm in case the target doesn't support it",
         || no_asm::check_crate(sess, &krate));

    // One final feature gating of the true AST that gets compiled
    // later, to make sure we've got everything (e.g. configuration
    // can insert new attributes via `cfg_attr`)
    try!(time(time_passes, "complete gated feature checking 2", || {
        sess.track_errors(|| {
            let features = syntax::feature_gate::check_crate(sess.codemap(),
                                                             &sess.parse_sess.span_diagnostic,
                                                             &krate,
                                                             &attributes,
                                                             sess.opts.unstable_features);
            *sess.features.borrow_mut() = features;
        })
    }));

    try!(time(time_passes,
              "const fn bodies and arguments",
              || const_fn::check_crate(sess, &krate)));

    if sess.opts.debugging_opts.input_stats {
        println!("Post-expansion node count: {}", count_nodes(&krate));
    }

    Ok(krate)
}

pub fn assign_node_ids(sess: &Session, krate: ast::Crate) -> ast::Crate {
    struct NodeIdAssigner<'a> {
        sess: &'a Session,
    }

    impl<'a> Folder for NodeIdAssigner<'a> {
        fn new_id(&mut self, old_id: ast::NodeId) -> ast::NodeId {
            assert_eq!(old_id, ast::DUMMY_NODE_ID);
            self.sess.next_node_id()
        }
    }

    let krate = time(sess.time_passes(),
                     "assigning node ids",
                     || NodeIdAssigner { sess: sess }.fold_crate(krate));

    if sess.opts.debugging_opts.ast_json {
        println!("{}", json::as_json(&krate));
    }

    krate
}

pub fn make_map<'ast>(sess: &Session,
                      forest: &'ast mut hir_map::Forest)
                      -> hir_map::Map<'ast> {
    // Construct the HIR map
    time(sess.time_passes(),
         "indexing hir",
         move || hir_map::map_crate(forest))
}

/// Run the resolution, typechecking, region checking and other
/// miscellaneous analysis passes on the crate. Return various
/// structures carrying the results of the analysis.
pub fn phase_3_run_analysis_passes<'tcx, F, R>(sess: &'tcx Session,
                                               cstore: &CStore,
                                               hir_map: hir_map::Map<'tcx>,
                                               arenas: &'tcx ty::CtxtArenas<'tcx>,
                                               name: &str,
                                               make_glob_map: resolve::MakeGlobMap,
                                               f: F)
                                               -> Result<R, usize>
    where F: FnOnce(&TyCtxt<'tcx>, Option<MirMap<'tcx>>, ty::CrateAnalysis, CompileResult) -> R
{
    macro_rules! try_with_f {
        ($e: expr, ($t: expr, $m: expr, $a: expr)) => {
            match $e {
                Ok(x) => x,
                Err(x) => {
                    f($t, $m, $a, Err(x));
                    return Err(x);
                }
            }
        }
    }

    let time_passes = sess.time_passes();

    time(time_passes,
         "external crate/lib resolution",
         || LocalCrateReader::new(sess, cstore, &hir_map).read_crates());

    let lang_items = try!(time(time_passes, "language item collection", || {
        sess.track_errors(|| {
            middle::lang_items::collect_language_items(&sess, &hir_map)
        })
    }));

    let resolve::CrateMap {
        def_map,
        freevars,
        export_map,
        trait_map,
        glob_map,
    } = time(time_passes,
             "resolution",
             || resolve::resolve_crate(sess, &hir_map, make_glob_map));

    let mut analysis = ty::CrateAnalysis {
        export_map: export_map,
        access_levels: AccessLevels::default(),
        reachable: NodeSet(),
        name: name,
        glob_map: glob_map,
    };

    let named_region_map = try!(time(time_passes,
                                     "lifetime resolution",
                                     || middle::resolve_lifetime::krate(sess,
                                                                        &hir_map,
                                                                        &def_map.borrow())));

    time(time_passes,
         "looking for entry point",
         || middle::entry::find_entry_point(sess, &hir_map));

    sess.plugin_registrar_fn.set(time(time_passes, "looking for plugin registrar", || {
        plugin::build::find_plugin_registrar(sess.diagnostic(), &hir_map)
    }));

    let region_map = time(time_passes,
                          "region resolution",
                          || middle::region::resolve_crate(sess, &hir_map));

    time(time_passes,
         "loop checking",
         || loops::check_crate(sess, &hir_map));

    try!(time(time_passes,
              "static item recursion checking",
              || static_recursion::check_crate(sess, &def_map.borrow(), &hir_map)));

    let index = stability::Index::new(&hir_map);

    TyCtxt::create_and_enter(sess,
                               arenas,
                               def_map,
                               named_region_map,
                               hir_map,
                               freevars,
                               region_map,
                               lang_items,
                               index,
                               |tcx| {
        // passes are timed inside typeck
        try_with_f!(typeck::check_crate(tcx, trait_map), (tcx, None, analysis));

        time(time_passes,
             "const checking",
             || consts::check_crate(tcx));

        analysis.access_levels =
            time(time_passes, "privacy checking", || {
                rustc_privacy::check_crate(tcx, &analysis.export_map)
            });

        // Do not move this check past lint
        time(time_passes, "stability index", || {
            tcx.stability.borrow_mut().build(tcx, &analysis.access_levels)
        });

        time(time_passes,
             "intrinsic checking",
             || middle::intrinsicck::check_crate(tcx));

        time(time_passes,
             "effect checking",
             || middle::effect::check_crate(tcx));

        time(time_passes,
             "match checking",
             || middle::check_match::check_crate(tcx));

        // this must run before MIR dump, because
        // "not all control paths return a value" is reported here.
        //
        // maybe move the check to a MIR pass?
        time(time_passes,
             "liveness checking",
             || middle::liveness::check_crate(tcx));

        time(time_passes,
             "rvalue checking",
             || rvalues::check_crate(tcx));

        let mut mir_map =
            time(time_passes,
                 "MIR dump",
                 || mir::mir_map::build_mir_for_crate(tcx));

        time(time_passes, "MIR passes", || {
            let mut passes = sess.mir_passes.borrow_mut();
            // Push all the built-in passes.
            passes.push_pass(box mir::transform::remove_dead_blocks::RemoveDeadBlocks);
            passes.push_pass(box mir::transform::type_check::TypeckMir);
            passes.push_pass(box mir::transform::simplify_cfg::SimplifyCfg);
            // Late passes
            passes.push_pass(box mir::transform::no_landing_pads::NoLandingPads);
            passes.push_pass(box mir::transform::remove_dead_blocks::RemoveDeadBlocks);
            passes.push_pass(box mir::transform::erase_regions::EraseRegions);
            // And run everything.
            passes.run_passes(tcx, &mut mir_map);
        });

        time(time_passes,
             "borrow checking",
             || borrowck::check_crate(tcx));

        // Avoid overwhelming user with errors if type checking failed.
        // I'm not sure how helpful this is, to be honest, but it avoids
        // a
        // lot of annoying errors in the compile-fail tests (basically,
        // lint warnings and so on -- kindck used to do this abort, but
        // kindck is gone now). -nmatsakis
        if sess.err_count() > 0 {
            return Ok(f(tcx, Some(mir_map), analysis, Err(sess.err_count())));
        }

        analysis.reachable =
            time(time_passes,
                 "reachability checking",
                 || reachable::find_reachable(tcx, &analysis.access_levels));

        time(time_passes, "death checking", || {
            middle::dead::check_crate(tcx, &analysis.access_levels);
        });

        let ref lib_features_used =
            time(time_passes,
                 "stability checking",
                 || stability::check_unstable_api_usage(tcx));

        time(time_passes, "unused lib feature checking", || {
            stability::check_unused_or_stable_features(&tcx.sess,
                                                       lib_features_used)
        });

        time(time_passes,
             "lint checking",
             || lint::check_crate(tcx, &analysis.access_levels));

        // The above three passes generate errors w/o aborting
        if sess.err_count() > 0 {
            return Ok(f(tcx, Some(mir_map), analysis, Err(sess.err_count())));
        }

        Ok(f(tcx, Some(mir_map), analysis, Ok(())))
    })
}

/// Run the translation phase to LLVM, after which the AST and analysis can
pub fn phase_4_translate_to_llvm<'tcx>(tcx: &TyCtxt<'tcx>,
                                       mir_map: MirMap<'tcx>,
                                       analysis: ty::CrateAnalysis)
                                       -> trans::CrateTranslation {
    let time_passes = tcx.sess.time_passes();

    time(time_passes,
         "resolving dependency formats",
         || dependency_format::calculate(&tcx.sess));

    // Option dance to work around the lack of stack once closures.
    time(time_passes,
         "translation",
         move || trans::trans_crate(tcx, &mir_map, analysis))
}

/// Run LLVM itself, producing a bitcode file, assembly file or object file
/// as a side effect.
pub fn phase_5_run_llvm_passes(sess: &Session,
                               trans: &trans::CrateTranslation,
                               outputs: &OutputFilenames) -> CompileResult {
    if sess.opts.cg.no_integrated_as {
        let mut map = HashMap::new();
        map.insert(OutputType::Assembly, None);
        time(sess.time_passes(),
             "LLVM passes",
             || write::run_passes(sess, trans, &map, outputs));

        write::run_assembler(sess, outputs);

        // Remove assembly source, unless --save-temps was specified
        if !sess.opts.cg.save_temps {
            fs::remove_file(&outputs.temp_path(OutputType::Assembly)).unwrap();
        }
    } else {
        time(sess.time_passes(),
             "LLVM passes",
             || write::run_passes(sess, trans, &sess.opts.output_types, outputs));
    }

    if sess.err_count() > 0 {
        Err(sess.err_count())
    } else {
        Ok(())
    }
}

/// Run the linker on any artifacts that resulted from the LLVM run.
/// This should produce either a finished executable or library.
pub fn phase_6_link_output(sess: &Session,
                           trans: &trans::CrateTranslation,
                           outputs: &OutputFilenames) {
    time(sess.time_passes(),
         "linking",
         || link::link_binary(sess, trans, outputs, &trans.link.crate_name));
}

fn escape_dep_filename(filename: &str) -> String {
    // Apparently clang and gcc *only* escape spaces:
    // http://llvm.org/klaus/clang/commit/9d50634cfc268ecc9a7250226dd5ca0e945240d4
    filename.replace(" ", "\\ ")
}

fn write_out_deps(sess: &Session, outputs: &OutputFilenames, id: &str) {
    let mut out_filenames = Vec::new();
    for output_type in sess.opts.output_types.keys() {
        let file = outputs.path(*output_type);
        match *output_type {
            OutputType::Exe => {
                for output in sess.crate_types.borrow().iter() {
                    let p = link::filename_for_input(sess, *output, id, outputs);
                    out_filenames.push(p);
                }
            }
            _ => {
                out_filenames.push(file);
            }
        }
    }

    // Write out dependency rules to the dep-info file if requested
    if !sess.opts.output_types.contains_key(&OutputType::DepInfo) {
        return;
    }
    let deps_filename = outputs.path(OutputType::DepInfo);

    let result =
        (|| -> io::Result<()> {
            // Build a list of files used to compile the output and
            // write Makefile-compatible dependency rules
            let files: Vec<String> = sess.codemap()
                                         .files
                                         .borrow()
                                         .iter()
                                         .filter(|fmap| fmap.is_real_file())
                                         .filter(|fmap| !fmap.is_imported())
                                         .map(|fmap| escape_dep_filename(&fmap.name))
                                         .collect();
            let mut file = try!(fs::File::create(&deps_filename));
            for path in &out_filenames {
                try!(write!(file, "{}: {}\n\n", path.display(), files.join(" ")));
            }

            // Emit a fake target for each input file to the compilation. This
            // prevents `make` from spitting out an error if a file is later
            // deleted. For more info see #28735
            for path in files {
                try!(writeln!(file, "{}:", path));
            }
            Ok(())
        })();

    match result {
        Ok(()) => {}
        Err(e) => {
            sess.fatal(&format!("error writing dependencies to `{}`: {}",
                                deps_filename.display(),
                                e));
        }
    }
}

pub fn collect_crate_types(session: &Session, attrs: &[ast::Attribute]) -> Vec<config::CrateType> {
    // Unconditionally collect crate types from attributes to make them used
    let attr_types: Vec<config::CrateType> =
        attrs.iter()
             .filter_map(|a| {
                 if a.check_name("crate_type") {
                     match a.value_str() {
                         Some(ref n) if *n == "rlib" => {
                             Some(config::CrateTypeRlib)
                         }
                         Some(ref n) if *n == "dylib" => {
                             Some(config::CrateTypeDylib)
                         }
                         Some(ref n) if *n == "lib" => {
                             Some(config::default_lib_output())
                         }
                         Some(ref n) if *n == "staticlib" => {
                             Some(config::CrateTypeStaticlib)
                         }
                         Some(ref n) if *n == "bin" => Some(config::CrateTypeExecutable),
                         Some(_) => {
                             session.add_lint(lint::builtin::UNKNOWN_CRATE_TYPES,
                                              ast::CRATE_NODE_ID,
                                              a.span,
                                              "invalid `crate_type` value".to_string());
                             None
                         }
                         _ => {
                             session.struct_span_err(a.span, "`crate_type` requires a value")
                                 .note("for example: `#![crate_type=\"lib\"]`")
                                 .emit();
                             None
                         }
                     }
                 } else {
                     None
                 }
             })
             .collect();

    // If we're generating a test executable, then ignore all other output
    // styles at all other locations
    if session.opts.test {
        return vec![config::CrateTypeExecutable];
    }

    // Only check command line flags if present. If no types are specified by
    // command line, then reuse the empty `base` Vec to hold the types that
    // will be found in crate attributes.
    let mut base = session.opts.crate_types.clone();
    if base.is_empty() {
        base.extend(attr_types);
        if base.is_empty() {
            base.push(link::default_output_for_target(session));
        }
        base.sort();
        base.dedup();
    }

    base.into_iter()
        .filter(|crate_type| {
            let res = !link::invalid_output_for_target(session, *crate_type);

            if !res {
                session.warn(&format!("dropping unsupported crate type `{}` for target `{}`",
                                      *crate_type,
                                      session.opts.target_triple));
            }

            res
        })
        .collect()
}

pub fn collect_crate_metadata(session: &Session, _attrs: &[ast::Attribute]) -> Vec<String> {
    session.opts.cg.metadata.clone()
}

pub fn build_output_filenames(input: &Input,
                              odir: &Option<PathBuf>,
                              ofile: &Option<PathBuf>,
                              attrs: &[ast::Attribute],
                              sess: &Session)
                              -> OutputFilenames {
    match *ofile {
        None => {
            // "-" as input file will cause the parser to read from stdin so we
            // have to make up a name
            // We want to toss everything after the final '.'
            let dirpath = match *odir {
                Some(ref d) => d.clone(),
                None => PathBuf::new(),
            };

            // If a crate name is present, we use it as the link name
            let stem = sess.opts
                           .crate_name
                           .clone()
                           .or_else(|| attr::find_crate_name(attrs).map(|n| n.to_string()))
                           .unwrap_or(input.filestem());

            OutputFilenames {
                out_directory: dirpath,
                out_filestem: stem,
                single_output_file: None,
                extra: sess.opts.cg.extra_filename.clone(),
                outputs: sess.opts.output_types.clone(),
            }
        }

        Some(ref out_file) => {
            let unnamed_output_types = sess.opts
                                           .output_types
                                           .values()
                                           .filter(|a| a.is_none())
                                           .count();
            let ofile = if unnamed_output_types > 1 {
                sess.warn("ignoring specified output filename because multiple outputs were \
                           requested");
                None
            } else {
                Some(out_file.clone())
            };
            if *odir != None {
                sess.warn("ignoring --out-dir flag due to -o flag.");
            }

            let cur_dir = Path::new("");

            OutputFilenames {
                out_directory: out_file.parent().unwrap_or(cur_dir).to_path_buf(),
                out_filestem: out_file.file_stem()
                                      .unwrap_or(OsStr::new(""))
                                      .to_str()
                                      .unwrap()
                                      .to_string(),
                single_output_file: ofile,
                extra: sess.opts.cg.extra_filename.clone(),
                outputs: sess.opts.output_types.clone(),
            }
        }
    }
}
