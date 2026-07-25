//! Port of esbuild's public `pkg/api` package.

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
#[repr(u16)]
pub enum Loader {
    #[default]
    None,
    Base64,
    Binary,
    Copy,
    Css,
    DataUrl,
    Default,
    Empty,
    File,
    GlobalCss,
    Js,
    Json,
    Jsx,
    LocalCss,
    Text,
    Ts,
    Tsx,
}
