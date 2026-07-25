// Port of upstream internal/helpers/path.go.

#[must_use]
pub fn is_inside_node_modules(mut path: &str) -> bool {
    loop {
        // This is intentionally platform-independent because user-specified
        // paths may use either slash style on any host platform.
        let Some(slash) = path.rfind(['/', '\\']) else {
            return false;
        };
        let (directory, base_with_slash) = path.split_at(slash);
        if &base_with_slash[1..] == "node_modules" {
            return true;
        }
        path = directory;
    }
}

/// The subset of Go's `url.URL` used by esbuild's path helpers.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FileUrl {
    pub scheme: String,
    pub host: String,
    pub path: String,
}

#[must_use]
pub fn is_file_url(file_url: &FileUrl) -> bool {
    file_url.scheme == "file"
        && (file_url.host.is_empty() || file_url.host == "localhost")
        && file_url.path.starts_with('/')
}

#[must_use]
pub fn file_url_from_file_path(file_path: &str) -> FileUrl {
    let mut path = file_path.replace('\\', "/");
    if !path.starts_with('/') {
        path.insert(0, '/');
    }
    FileUrl {
        scheme: "file".to_string(),
        host: String::new(),
        path,
    }
}

#[must_use]
pub fn file_path_from_file_url(cwd: &str, file_url: &FileUrl) -> String {
    let mut path = file_url.path.clone();
    if !cwd.starts_with('/') {
        if let Some(without_slash) = path.strip_prefix('/') {
            path = without_slash.to_string();
        }
        path = path.replace('/', "\\");
    }
    path
}

#[cfg(test)]
mod tests {
    use super::{
        FileUrl, file_path_from_file_url, file_url_from_file_path, is_file_url,
        is_inside_node_modules,
    };

    #[test]
    fn detects_node_modules_with_both_slash_styles() {
        assert!(is_inside_node_modules("/a/node_modules/pkg/file.js"));
        assert!(is_inside_node_modules(r"C:\a\node_modules\pkg\file.js"));
        assert!(is_inside_node_modules("https://x/node_modules/pkg"));
        assert!(!is_inside_node_modules("/a/node_modulesx/pkg"));
        assert!(!is_inside_node_modules("node_modules"));
    }

    #[test]
    fn converts_unix_and_windows_file_paths() {
        let unix = file_url_from_file_path("/Users/User/Desktop");
        assert_eq!(unix.path, "/Users/User/Desktop");
        assert!(is_file_url(&unix));
        assert_eq!(
            file_path_from_file_url("/Users/User", &unix),
            "/Users/User/Desktop"
        );

        let windows = file_url_from_file_path(r"C:\Users\User\Desktop");
        assert_eq!(windows.path, "/C:/Users/User/Desktop");
        assert_eq!(
            file_path_from_file_url(r"C:\Users\User", &windows),
            r"C:\Users\User\Desktop"
        );
    }

    #[test]
    fn validates_file_url_components() {
        assert!(is_file_url(&FileUrl {
            scheme: "file".into(),
            host: "localhost".into(),
            path: "/x".into(),
        }));
        assert!(!is_file_url(&FileUrl {
            scheme: "https".into(),
            host: String::new(),
            path: "/x".into(),
        }));
    }
}
