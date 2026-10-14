//! App names double as OpenTunnel route labels, Docker name parts, and
//! Postgres role/database names, so they follow the strictest of those rules.

pub const MAX_APP_NAME: usize = 40;

/// Names that would collide with hedgehog's own resources.
const RESERVED: &[&str] = &["hedgehog", "postgres", "template0", "template1"];

/// Lowercase letters, digits, and inner hyphens, 1 to 40 characters.
pub fn is_valid_app_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let valid = |byte: &u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    let shape_ok = match bytes {
        [] => false,
        [only] => valid(only),
        [first, middle @ .., last] => {
            bytes.len() <= MAX_APP_NAME
                && valid(first)
                && valid(last)
                && middle.iter().all(|byte| valid(byte) || *byte == b'-')
        }
    };
    shape_ok && !RESERVED.contains(&name)
}

/// Turns a folder name like `My_Budget App` into an app name like
/// `my-budget-app`, or `None` if nothing usable is left.
pub fn app_name_from_dir(dir_name: &str) -> Option<String> {
    let mut name = String::with_capacity(dir_name.len());
    for char in dir_name.chars() {
        if char.is_ascii_alphanumeric() {
            name.push(char.to_ascii_lowercase());
        } else if !name.ends_with('-') {
            name.push('-');
        }
    }
    let mut name = name.trim_matches('-').to_string();
    name.truncate(MAX_APP_NAME);
    let name = name.trim_end_matches('-').to_string();
    is_valid_app_name(&name).then_some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_names() {
        assert!(is_valid_app_name("budget"));
        assert!(is_valid_app_name("my-app-2"));
        assert!(is_valid_app_name("a"));
        assert!(!is_valid_app_name(""));
        assert!(!is_valid_app_name("-budget"));
        assert!(!is_valid_app_name("budget-"));
        assert!(!is_valid_app_name("Budget"));
        assert!(!is_valid_app_name("my_app"));
        assert!(!is_valid_app_name("postgres"));
        assert!(!is_valid_app_name(&"a".repeat(41)));
    }

    #[test]
    fn derives_names_from_folders() {
        assert_eq!(app_name_from_dir("budget").as_deref(), Some("budget"));
        assert_eq!(
            app_name_from_dir("My_Budget App").as_deref(),
            Some("my-budget-app")
        );
        assert_eq!(
            app_name_from_dir("--weird..name--").as_deref(),
            Some("weird-name")
        );
        assert_eq!(app_name_from_dir("___"), None);
        assert_eq!(app_name_from_dir("postgres"), None);
        let long = format!("{}-tail", "a".repeat(39));
        assert_eq!(
            app_name_from_dir(&long).as_deref(),
            Some("a".repeat(39).as_str())
        );
    }
}
