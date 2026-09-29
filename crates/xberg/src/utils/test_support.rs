//! Run one ignored test in a fresh copy of the test binary, with environment variables set only there.
//! A test uses this instead of `set_var`, which races the tests on sibling threads. ~keep

use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::Command;

/// What [`child_test_root`] prints when the root is absent, and what [`run_child_test`] looks for.
const CHILD_SKIP_MARKER: &str = "SKIP: this test runs only in a child process that its parent test starts";

/// Run the ignored test `test_name` in a new process and fail unless exactly that one test ran and passed.
///
/// `root_variable` is set to `root` in the child; the child reads it back with [`child_test_root`].
/// `configure` sets any further variables on the child command.
///
/// A filter that matches no test makes libtest print `0 passed` and exit 0, so the exit status alone
/// cannot tell a passing child from a renamed one. The result line is the check. ~keep
///
/// A child that did not find its root returns early and still counts as passed, so the parent also
/// fails when the child printed the skip marker. ~keep
pub(crate) fn run_child_test(
    test_name: &str,
    root_variable: &str,
    root: impl AsRef<OsStr>,
    configure: impl FnOnce(&mut Command),
) {
    let mut command = Command::new(std::env::current_exe().expect("find the test binary"));
    command
        .arg("--exact")
        .arg(test_name)
        .arg("--ignored")
        .arg("--nocapture")
        .arg("--test-threads=1")
        .env(root_variable, root);
    configure(&mut command);
    let output = command.output().expect("start the child test process");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let ran_one = stdout
        .lines()
        .any(|line| line.starts_with("test result: ok. 1 passed; 0 failed;"));
    assert!(
        output.status.success() && ran_one,
        "child test {test_name} did not run exactly one passing test ({})\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
        output.status,
    );
    assert!(
        !stderr.contains(CHILD_SKIP_MARKER),
        "child test {test_name} returned early because it did not read {root_variable}\n--- stderr ---\n{stderr}",
    );
}

/// Read the root a parent test passed to its child, or `None` when the child runs without its parent.
///
/// `cargo test -- --ignored` and `--include-ignored` run the child in the parent process, where the
/// variable is absent. The child then returns early instead of panicking on the missing root. ~keep
pub(crate) fn child_test_root(root_variable: &str) -> Option<PathBuf> {
    let root = std::env::var_os(root_variable).map(PathBuf::from);
    if root.is_none() {
        eprintln!("{CHILD_SKIP_MARKER}");
    }
    root
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROBE_ROOT: &str = "XBERG_CHILD_TEST_SUPPORT_ROOT";
    const PROBE_EXTRA: &str = "XBERG_CHILD_TEST_SUPPORT_EXTRA";

    #[test]
    fn a_child_that_runs_and_passes_satisfies_the_parent() {
        let root = tempfile::TempDir::new().unwrap();
        run_child_test(
            "utils::test_support::tests::probe_child",
            PROBE_ROOT,
            root.path(),
            |command| {
                command.env(PROBE_EXTRA, "set by configure");
            },
        );
    }

    #[test]
    #[should_panic(expected = "did not run exactly one passing test")]
    fn a_filter_that_matches_no_test_fails_the_parent() {
        let root = tempfile::TempDir::new().unwrap();
        run_child_test(
            "utils::test_support::tests::no_such_child",
            PROBE_ROOT,
            root.path(),
            |_| {},
        );
    }

    #[test]
    #[should_panic(expected = "did not run exactly one passing test")]
    fn a_failing_child_fails_the_parent() {
        let root = tempfile::TempDir::new().unwrap();
        run_child_test(
            "utils::test_support::tests::probe_child",
            PROBE_ROOT,
            root.path(),
            |_| {},
        );
    }

    #[test]
    #[should_panic(expected = "returned early because it did not read")]
    fn a_child_that_does_not_read_the_root_variable_fails_the_parent() {
        let root = tempfile::TempDir::new().unwrap();
        run_child_test(
            "utils::test_support::tests::probe_child",
            "XBERG_CHILD_TEST_SUPPORT_ROOT_LOST",
            root.path(),
            |command| {
                command.env(PROBE_EXTRA, "set by configure");
            },
        );
    }

    #[test]
    #[should_panic(expected = "did not run exactly one passing test")]
    fn a_child_that_prints_a_passing_line_and_exits_nonzero_fails_the_parent() {
        let root = tempfile::TempDir::new().unwrap();
        run_child_test(
            "utils::test_support::tests::passing_line_then_exit_child",
            PROBE_ROOT,
            root.path(),
            |_| {},
        );
    }

    #[test]
    fn a_child_started_without_its_root_returns_early() {
        assert_eq!(child_test_root(PROBE_ROOT), None);
        probe_child();
    }

    #[test]
    #[ignore = "run in an isolated subprocess by the run_child_test tests"]
    fn probe_child() {
        let Some(root) = child_test_root(PROBE_ROOT) else {
            return;
        };
        assert!(root.is_dir(), "the parent passes its temporary directory");
        assert_eq!(
            std::env::var(PROBE_EXTRA).as_deref(),
            Ok("set by configure"),
            "configure sets variables on the child"
        );
    }

    #[test]
    #[ignore = "run in an isolated subprocess by the run_child_test tests"]
    fn passing_line_then_exit_child() {
        if child_test_root(PROBE_ROOT).is_none() {
            return;
        }
        // libtest has already printed this test's name without a newline, so start a fresh line. ~keep
        println!("\ntest result: ok. 1 passed; 0 failed; 0 ignored");
        std::process::exit(3);
    }
}
