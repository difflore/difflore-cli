use std::fmt;
use std::io::{self, Write};

pub(crate) fn safe_print(args: fmt::Arguments<'_>) {
    write_stdout(args, false);
}

pub(crate) fn safe_println(args: fmt::Arguments<'_>) {
    write_stdout(args, true);
}

fn write_stdout(args: fmt::Arguments<'_>, newline: bool) {
    let stdout = io::stdout();
    let mut lock = stdout.lock();
    let result = if newline {
        lock.write_fmt(args).and_then(|()| lock.write_all(b"\n"))
    } else {
        lock.write_fmt(args)
    };
    if let Err(err) = result {
        handle_stdout_error(&err);
    }
}

fn handle_stdout_error(err: &io::Error) {
    if is_broken_stdout_pipe(err) {
        crate::support::util::exit_code(0);
    }
    eprintln!("error: failed printing to stdout: {err}");
    crate::support::util::exit_code(1);
}

pub(crate) fn is_broken_stdout_pipe(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::BrokenPipe {
        return true;
    }
    is_windows_broken_stdout_pipe(err)
}

#[cfg(windows)]
fn is_windows_broken_stdout_pipe(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(109 | 232 | 233))
}

#[cfg(not(windows))]
fn is_windows_broken_stdout_pipe(_: &io::Error) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_standard_broken_pipe_error_kind() {
        let err = io::Error::from(io::ErrorKind::BrokenPipe);

        assert!(is_broken_stdout_pipe(&err));
    }

    #[cfg(windows)]
    #[test]
    fn detects_windows_pipe_closed_errors() {
        for code in [109, 232, 233] {
            let err = io::Error::from_raw_os_error(code);

            assert!(is_broken_stdout_pipe(&err), "code {code}");
        }
    }
}
