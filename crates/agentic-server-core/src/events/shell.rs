/// One command's incremental lifecycle within a shell output item.
#[derive(Debug, Clone)]
pub enum ShellCommandUpdate {
    Added(String),
    Delta(String),
    Done(String),
}
