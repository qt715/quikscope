// Cocoa's platform requires the process main thread, even for headless rendering.
// A single-thread libtest-mimic runner retains filtering and panic-based failures.
macro_rules! main_thread_tests {
    ($($case:ident),+ $(,)?) => {
        fn main() {
            let mut args = libtest_mimic::Arguments::from_args();
            args.test_threads = Some(1);
            #[cfg(not(windows))]
            let trials = vec![$(
                libtest_mimic::Trial::test(stringify!($case), || {
                    cases::$case();
                    Ok(())
                }),
            )+];
            // GPUI has no Windows offscreen renderer; keep the existing zero-case suites.
            #[cfg(windows)]
            let trials = Vec::new();
            libtest_mimic::run(&args, trials).exit();
        }
    };
}
