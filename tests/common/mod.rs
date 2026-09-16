//! Helpers shared by the integration tests.

#[cfg(unix)]
/// Write `contents` to `path` as an executable (mode 0755) script.
///
/// On Linux, `execve` fails with `ETXTBSY` while any process holds the file
/// open for writing. Writing the shim from this multithreaded test process
/// leaves a window in which a child forked by a concurrent test inherits the
/// writable descriptor and keeps it until that child execs, so a spawn of the
/// shim right afterwards can fail with "Text file busy". Renaming the file
/// into place does not help, because the inherited descriptor still refers
/// to the same inode. Instead, a separate `sh` process creates and fills the
/// file, so this process never holds a writable descriptor to it and no fork
/// can leak one. The helper waits for `sh` to exit, after which no process
/// has the file open for writing.
pub fn write_executable_shim(path: &std::path::Path, contents: impl AsRef<[u8]>) {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("/bin/sh")
        .arg("-c")
        .arg(r#"cat > "$1" && chmod 755 "$1""#)
        .arg("sh")
        .arg(path)
        .stdin(Stdio::piped())
        .spawn()
        .expect("spawn sh to write the shim");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(contents.as_ref())
        .expect("pipe shim contents to sh");
    let status = child.wait().expect("wait for sh writing the shim");
    assert!(
        status.success(),
        "writing shim {} failed: {status}",
        path.display()
    );
}
