use crate::args::{Args, Border, Layout};
use crate::highlight;
use crate::palette::{Highlights, Palette};
use serde::Deserialize;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// The settings in the config file. Anything left out stays `None` and falls
/// through to the built-in default.
#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
  ui: Ui,
  diff: Diff,
  syntax: Syntax,
  watch: Watch,
  highlights: Highlights,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct Ui {
  border: Option<Border>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct Diff {
  layout: Option<Layout>,
  context: Option<u32>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct Syntax {
  enabled: Option<bool>,
  theme: Option<String>,
}

#[derive(Debug, Default, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
struct Watch {
  enabled: Option<bool>,
}

/// The config file's path when no `--config` is given.
fn default_path(xdg_config_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
  let base = match xdg_config_home.filter(|s| !s.is_empty()) {
    Some(dir) => PathBuf::from(dir),
    None => Path::new(home.filter(|s| !s.is_empty())?).join(".config"),
  };
  Some(base.join("dunk").join("config.toml"))
}

impl Config {
  fn parse(text: &str) -> Result<Self, String> {
    let config: Self = toml::from_str(text).map_err(|e| e.to_string())?;
    if let Some(theme) = &config.syntax.theme {
      highlight::resolve_theme(theme).map_err(|e| format!("syntax.theme: {e}"))?;
    }
    config.highlights.palette()?;
    Ok(config)
  }

  /// Reads `explicit`, or the default path when there is none. A missing
  /// default file is not an error, a missing explicit one is.
  pub fn load(explicit: Option<&Path>) -> Result<Self, String> {
    let path = if let Some(path) = explicit { path.to_path_buf() } else {
      let xdg = std::env::var("XDG_CONFIG_HOME").ok();
      let home = std::env::var("HOME").ok();
      match default_path(xdg.as_deref(), home.as_deref()) {
        Some(path) => path,
        None => return Ok(Self::default()),
      }
    };
    match fs::read_to_string(&path) {
      Ok(text) => Self::parse(&text).map_err(|e| format!("{}: {e}", path.display())),
      Err(e) if e.kind() == io::ErrorKind::NotFound && explicit.is_none() => Ok(Self::default()),
      Err(e) => Err(format!("{}: {e}", path.display())),
    }
  }
}

/// The settings after the command line, the config file and the built-in
/// defaults are combined, in that order of priority.
#[derive(Debug, PartialEq, Eq)]
pub struct Settings {
  pub layout: Layout,
  pub border: Border,
  pub context: Option<u32>,
  /// `None` when syntax highlighting is off.
  pub theme: Option<String>,
  pub watch: bool,
  pub palette: Palette,
}

impl Settings {
  pub fn resolve(args: &Args, config: Config) -> Self {
    let syntax = args.syntax.or(config.syntax.enabled).unwrap_or(true);
    let theme = args.theme.clone().or(config.syntax.theme);
    Self {
      layout: args
        .layout
        .or(config.diff.layout)
        .unwrap_or(Layout::Unified),
      border: args.border.or(config.ui.border).unwrap_or_default(),
      context: args.context.or(config.diff.context),
      theme: syntax.then(|| {
        theme
          .and_then(|t| highlight::resolve_theme(&t).ok())
          .unwrap_or_else(|| highlight::DEFAULT_THEME.to_string())
      }),
      watch: args.watch.or(config.watch.enabled).unwrap_or(false),
      palette: config.highlights.palette().unwrap_or_default(),
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  const FULL: &str = r#"
[ui]
border = "rounded"

[diff]
layout = "split"
context = 5

[syntax]
enabled = false
theme = "nord"

[watch]
enabled = true
"#;

  fn args(extra: &[&str]) -> Args {
    let mut argv = vec!["dunk"];
    argv.extend_from_slice(extra);
    Args::parse_from(&argv)
  }

  #[test]
  fn an_empty_file_sets_nothing() {
    assert_eq!(Config::parse("").unwrap(), Config::default());
  }

  #[test]
  fn a_partial_file_sets_only_what_it_names() {
    let config = Config::parse("[diff]\ncontext = 2\n").unwrap();
    assert_eq!(config.diff.context, Some(2));
    assert_eq!(config.diff.layout, None);
    assert_eq!(config.watch, Watch::default());
  }

  #[test]
  fn a_full_file_sets_every_setting() {
    let s = Settings::resolve(&args(&[]), Config::parse(FULL).unwrap());
    assert_eq!(s.layout, Layout::Split);
    assert_eq!(s.border, Border::Rounded);
    assert_eq!(s.context, Some(5));
    assert_eq!(s.theme, None);
    assert!(s.watch);
  }

  #[test]
  fn unknown_keys_and_sections_are_errors() {
    let key = Config::parse("[diff]\nlayuot = \"split\"\n").unwrap_err();
    assert!(key.contains("layuot"), "{key}");
    let section = Config::parse("[colors]\nadded = \"green\"\n").unwrap_err();
    assert!(section.contains("colors"), "{section}");
  }

  #[test]
  fn a_wrong_type_names_the_key() {
    let err = Config::parse("[diff]\nlayout = \"sideways\"\n").unwrap_err();
    assert!(err.contains("sideways"), "{err}");
    let err = Config::parse("[diff]\ncontext = \"many\"\n").unwrap_err();
    assert!(err.contains("context"), "{err}");
  }

  #[test]
  fn the_theme_is_validated_ignoring_case() {
    let err = Config::parse("[syntax]\ntheme = \"nope\"\n").unwrap_err();
    assert!(err.contains("syntax.theme: unknown theme 'nope'"), "{err}");
    let s = Settings::resolve(
      &args(&[]),
      Config::parse("[syntax]\ntheme = \"NORD\"\n").unwrap(),
    );
    assert_eq!(s.theme.as_deref(), Some("Nord"));
  }

  #[test]
  fn defaults_apply_when_nothing_is_set() {
    let s = Settings::resolve(&args(&[]), Config::default());
    assert_eq!(
      s,
      Settings {
        layout: Layout::Unified,
        border: Border::Plain,
        context: None,
        theme: Some(highlight::DEFAULT_THEME.to_string()),
        watch: false,
        palette: Palette::default(),
      }
    );
  }

  #[test]
  fn flags_beat_the_config_file() {
    let flags = args(&[
      "--border",
      "plain",
      "--layout",
      "unified",
      "-U",
      "1",
      "--syntax",
      "--no-watch",
      "--theme",
      "dracula",
    ]);
    let s = Settings::resolve(&flags, Config::parse(FULL).unwrap());
    assert_eq!(s.layout, Layout::Unified);
    assert_eq!(s.border, Border::Plain);
    assert_eq!(s.context, Some(1));
    assert_eq!(s.theme.as_deref(), Some("Dracula"));
    assert!(!s.watch);
  }

  #[test]
  fn highlights_are_validated_and_reach_the_settings() {
    let text = "[highlights]\nDiffAdd = { bg = \"#102030\" }\n";
    let s = Settings::resolve(&args(&[]), Config::parse(text).unwrap());
    assert_eq!(
      s.palette.diff_add,
      ratatui::style::Style::new().bg(ratatui::style::Color::Rgb(16, 32, 48))
    );
    let err = Config::parse("[highlights]\nDiffAdd = { bg = \"grn\" }\n").unwrap_err();
    assert!(
      err.contains("highlights.DiffAdd.bg: unknown color 'grn'"),
      "{err}"
    );
    let err = Config::parse("[highlights]\nDiffAdds = { bg = \"red\" }\n").unwrap_err();
    assert!(
      err.contains("DiffAdds") && err.contains("DiffDeleteText"),
      "{err}"
    );
  }

  #[test]
  fn the_default_path_prefers_xdg_then_home() {
    assert_eq!(
      default_path(Some("/x"), Some("/h")),
      Some(PathBuf::from("/x/dunk/config.toml"))
    );
    assert_eq!(
      default_path(Some(""), Some("/h")),
      Some(PathBuf::from("/h/.config/dunk/config.toml"))
    );
    assert_eq!(default_path(None, None), None);
  }

  #[test]
  fn a_missing_default_file_is_fine_but_a_missing_explicit_one_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.toml");
    let err = Config::load(Some(&missing)).unwrap_err();
    assert!(err.contains("nope.toml"), "{err}");

    let file = dir.path().join("config.toml");
    fs::write(&file, FULL).unwrap();
    assert_eq!(Config::load(Some(&file)).unwrap().diff.context, Some(5));
    fs::write(&file, "[diff\n").unwrap();
    assert!(
      Config::load(Some(&file))
        .unwrap_err()
        .contains("config.toml")
    );
  }
}
