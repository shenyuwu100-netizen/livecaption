//! Desktop visibility policy shared by startup and single-instance recovery.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LaunchMode {
    Foreground,
    Background,
    HeadlessSync,
}

impl LaunchMode {
    pub fn from_args<I, S>(args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut mode = Self::Foreground;
        for arg in args {
            match arg.as_ref() {
                "--headless-sync" => return Self::HeadlessSync,
                "--background" => mode = Self::Background,
                _ => {}
            }
        }
        mode
    }

    pub const fn show_control_on_launch(self) -> bool {
        matches!(self, Self::Foreground)
    }

    pub const fn create_tray(self) -> bool {
        !matches!(self, Self::HeadlessSync)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_launch_keeps_the_control_window_and_tray() {
        let mode = LaunchMode::from_args(["livecaption.exe"]);
        assert_eq!(mode, LaunchMode::Foreground);
        assert!(mode.show_control_on_launch());
        assert!(mode.create_tray());
    }

    #[test]
    fn background_launch_keeps_the_tray() {
        let mode = LaunchMode::from_args(["livecaption.exe", "--background"]);
        assert_eq!(mode, LaunchMode::Background);
        assert!(!mode.show_control_on_launch());
        assert!(mode.create_tray());
    }

    #[test]
    fn headless_sync_launch_excludes_desktop_entry_points() {
        let mode = LaunchMode::from_args(["livecaption.exe", "--headless-sync"]);
        assert_eq!(mode, LaunchMode::HeadlessSync);
        assert!(!mode.show_control_on_launch());
        assert!(!mode.create_tray());
    }

    #[test]
    fn headless_sync_wins_over_background_in_either_argument_order() {
        for args in [
            ["--background", "--headless-sync"],
            ["--headless-sync", "--background"],
        ] {
            assert_eq!(LaunchMode::from_args(args), LaunchMode::HeadlessSync);
        }
    }

    #[test]
    fn empty_or_unrelated_arguments_keep_normal_launch() {
        assert_eq!(
            LaunchMode::from_args([] as [&str; 0]),
            LaunchMode::Foreground
        );
        assert_eq!(
            LaunchMode::from_args(["--diagnostics-file", "status.json"]),
            LaunchMode::Foreground
        );
    }

    #[test]
    fn accepts_owned_arguments_from_the_process_iterator() {
        let args = vec![
            "livecaption.exe".to_owned(),
            "--headless-sync".to_owned(),
        ];
        assert_eq!(LaunchMode::from_args(args), LaunchMode::HeadlessSync);
    }

    #[test]
    fn similar_argument_names_do_not_enable_background_modes() {
        assert_eq!(
            LaunchMode::from_args(["--background-extra", "--headless-sync=false"]),
            LaunchMode::Foreground
        );
    }
}
