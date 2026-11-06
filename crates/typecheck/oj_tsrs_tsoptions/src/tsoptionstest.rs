// Go package `tsoptions/tsoptionstest`: test helpers shared with other crates' tests.

use tsrs_core::P;
use tsrs_core::tspath;
use tsrs_vfs::FS;
use tsrs_vfs::vfstest;

use crate::parsedcommandline::ParsedCommandLine;
use crate::tsconfigparsing::{
    ParseConfigHost, new_tsconfig_source_file_from_file_path,
    parse_json_source_file_config_file_content,
};

pub fn get_parsed_command_line(
    json_text: &str,
    files: &[(&str, &str)],
    current_directory: &str,
    use_case_sensitive_file_names: bool,
) -> ParsedCommandLine {
    let host = new_vfs_parse_config_host(files, current_directory, use_case_sensitive_file_names);
    let config_file_name = tspath::combine_paths(current_directory, &["tsconfig.json"]);
    let tsconfig_source_file = new_tsconfig_source_file_from_file_path(
        &config_file_name,
        tspath::to_path(&config_file_name, current_directory, use_case_sensitive_file_names),
        json_text,
    );
    parse_json_source_file_config_file_content(
        tsconfig_source_file,
        host,
        current_directory,
        None,
        None,
        &config_file_name,
        &[],
        None,
    )
}

pub struct VfsParseConfigHost {
    pub vfs: &'static dyn FS,
    pub current_directory: String,
}

impl ParseConfigHost for VfsParseConfigHost {
    fn fs(&self) -> &dyn FS {
        self.vfs
    }

    fn get_current_directory(&self) -> &str {
        &self.current_directory
    }
}

// The hosts live in the arena for the test's life; config parsing only borrows them.
pub fn new_vfs_parse_config_host(
    files: &[(&str, &str)],
    current_directory: &str,
    use_case_sensitive_file_names: bool,
) -> &'static VfsParseConfigHost {
    P::new(VfsParseConfigHost {
        vfs: P::new(vfstest::from_map(
            files.iter().map(|(k, v)| (*k, v.to_string())),
            use_case_sensitive_file_names,
        ))
        .get(),
        current_directory: current_directory.to_string(),
    })
    .get()
}
