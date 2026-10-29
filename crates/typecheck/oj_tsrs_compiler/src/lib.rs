pub mod affinity;
mod checker_program;
pub mod checkerpool;
pub mod diagnosticwriter;
mod emithost;
mod emitter;
mod file_include;
pub mod fileloader;
pub mod fileregions;
mod filesparser;
mod host;
mod includeprocessor;
mod processing_diagnostic;
mod program;
pub mod program_emit;
mod programlike;
mod programregions;
pub mod projectreferencedtsfakinghost;
pub mod projectreferencefilemapper;
mod projectreferenceparser;
mod sharedgraph;
mod splitcheck;

pub use checkerpool::{
    Checker, CheckerHandle, CheckerPool, Context, PooledChecker, checker_count_upper_bound,
    parse_memory_size, set_checker_assignment_from_cli, set_checker_cost_cache_from_cli,
    set_max_memory_from_cli, use_go_default_checker_count,
};
pub use emitter::EmitOnly;
pub use file_include::FileIncludeReason;
pub use fileloader::{
    DuplicateSourceFile, LibFile, content_mapper_project_diagnostic,
    content_mapper_project_error_diagnostic,
};
pub use fileregions::{
    LeafMode, LeafSettings, bind_in_file_regions, enable as enable_file_regions, enable_lazy_dts,
    lazy_dts_allowed, leaf_settings_from_env, stats_report as leaf_stats_report,
};
pub use host::{
    CompilerHost, TraceFn, get_content_mapped_source_files_with, new_cached_fs_compiler_host,
    new_compiler_host,
};
pub use program::{
    CreateCheckerPool, CreateModuleResolver, Program, ProgramConfig, ProgramOptions,
    SharedProgramData, filter_no_emit_semantic_diagnostics, free_program, free_unshared_program,
    get_diagnostics_of_any_program, new_program, shared_program_data,
    sort_and_deduplicate_diagnostics, worker_pool,
};
pub use program_emit::{
    EmitOptions, EmitResult, SourceMapEmitResult, WriteFile, WriteFileData, combine_emit_results,
    handle_no_emit_options,
};
pub use programlike::{ProgramLike, get_diagnostics_of_any_program_like};
pub use programregions::ProgramRegions;
pub use tsrs_checker::sitecount::dump as dump_site_counts;
