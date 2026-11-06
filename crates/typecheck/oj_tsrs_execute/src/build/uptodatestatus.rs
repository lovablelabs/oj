// Port of execute/build/uptodatestatus.go.

use std::time::SystemTime;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum UpToDateStatusType {
    // Errors:

    // config file was not found
    ConfigFileNotFound,
    // found errors during build
    BuildErrors,
    // did not build because upstream project has errors - and we have option to stop build on upstream errors
    UpstreamErrors,

    // Its all good, no work to do
    UpToDate,

    // Pseudo-builds - touch timestamps, no actual build:

    // The project appears out of date because its upstream inputs are newer than its outputs,
    // but all of its outputs are actually newer than the previous identical outputs of its (.d.ts) inputs.
    // This means we can Pseudo-build (just touch timestamps), as if we had actually built this project.
    UpToDateWithUpstreamTypes,
    // The project appears up to date and even though input file changed, its text didnt so just need to update timestamps
    UpToDateWithInputFileText,

    // Needs build:

    // input file is missing
    InputFileMissing,
    // output file is missing
    OutputMissing,
    // input file is newer than output file
    InputFileNewer,
    // build info is out of date as we need to emit some files
    OutOfDateBuildInfoWithPendingEmit,
    // build info indicates that project has errors and they need to be reported
    OutOfDateBuildInfoWithErrors,
    // build info options indicate there is work to do based on changes in options
    OutOfDateOptions,
    // file was root when built but not any more
    OutOfDateRoots,
    // buildInfo.version mismatch with current ts version
    TsVersionOutputOfDate,
    // build because --force was specified
    ForceBuild,

    // solution file
    Solution,
}

#[derive(Clone, Debug)]
pub struct InputOutputName {
    pub(crate) input: String,
    pub(crate) output: String,
}

// Go's zero time.Time is None.
#[derive(Clone, Debug, Default)]
pub struct FileAndTime {
    pub(crate) file: String,
    pub(crate) time: Option<SystemTime>,
}

#[derive(Clone, Debug)]
pub struct InputOutputFileAndTime {
    pub(crate) input: FileAndTime,
    pub(crate) output: FileAndTime,
}

#[derive(Clone, Debug)]
pub struct UpstreamErrors {
    pub(crate) ref_: String,
    pub(crate) ref_has_upstream_errors: bool,
}

// Go `data any`.
#[derive(Clone, Debug)]
pub enum StatusData {
    None,
    String(String),
    InputOutputName(InputOutputName),
    InputOutputFileAndTime(InputOutputFileAndTime),
    UpstreamErrors(UpstreamErrors),
}

#[derive(Clone, Debug)]
pub struct UpToDateStatus {
    pub(crate) kind: UpToDateStatusType,
    pub(crate) data: StatusData,
}

impl UpToDateStatus {
    pub(crate) fn new(kind: UpToDateStatusType) -> UpToDateStatus {
        UpToDateStatus { kind, data: StatusData::None }
    }

    pub(crate) fn with(kind: UpToDateStatusType, data: StatusData) -> UpToDateStatus {
        UpToDateStatus { kind, data }
    }

    // uptodatestatus.go:83
    pub(crate) fn is_error(&self) -> bool {
        matches!(
            self.kind,
            UpToDateStatusType::ConfigFileNotFound
                | UpToDateStatusType::BuildErrors
                | UpToDateStatusType::UpstreamErrors
        )
    }

    // uptodatestatus.go:94
    pub(crate) fn is_pseudo_build(&self) -> bool {
        matches!(
            self.kind,
            UpToDateStatusType::UpToDateWithUpstreamTypes
                | UpToDateStatusType::UpToDateWithInputFileText
        )
    }

    // uptodatestatus.go:104
    pub(crate) fn input_output_file_and_time(&self) -> Option<&InputOutputFileAndTime> {
        match &self.data {
            StatusData::InputOutputFileAndTime(d) => Some(d),
            _ => None,
        }
    }

    // uptodatestatus.go:112
    pub(crate) fn input_output_name(&self) -> Option<&InputOutputName> {
        match &self.data {
            StatusData::InputOutputName(d) => Some(d),
            _ => None,
        }
    }

    // uptodatestatus.go:120
    pub(crate) fn oldest_output_file_name(&self) -> String {
        assert!(
            self.is_pseudo_build() || self.kind == UpToDateStatusType::UpToDate,
            "only valid for up to date status of pseudo-build or up to date"
        );

        if let Some(input_output_file_and_time) = self.input_output_file_and_time() {
            return input_output_file_and_time.output.file.clone();
        }
        if let Some(input_output_name) = self.input_output_name() {
            return input_output_name.output.clone();
        }
        match &self.data {
            StatusData::String(s) => s.clone(),
            _ => panic!("interface conversion: interface {{}} is nil, not string"),
        }
    }

    // uptodatestatus.go:134
    pub(crate) fn upstream_errors(&self) -> &UpstreamErrors {
        match &self.data {
            StatusData::UpstreamErrors(d) => d,
            _ => panic!("interface conversion: not *upstreamErrors"),
        }
    }

    pub(crate) fn data_string(&self) -> &str {
        match &self.data {
            StatusData::String(s) => s,
            _ => panic!("interface conversion: not string"),
        }
    }
}

// Go time.Time.After / Before with the zero time as the earliest instant.
pub(crate) fn after(a: Option<SystemTime>, b: Option<SystemTime>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        _ => false,
    }
}

pub(crate) fn before(a: Option<SystemTime>, b: Option<SystemTime>) -> bool {
    after(b, a)
}
