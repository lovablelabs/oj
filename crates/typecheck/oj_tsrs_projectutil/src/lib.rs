// Go internal/project/dirty and internal/project/logging: leaf packages that both internal/project and
// internal/ls/autoimport import (tsrs_ls sits below tsrs_project, so they cannot live in tsrs_project).

pub mod dirty;
pub mod logging;
