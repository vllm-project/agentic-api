//! Which media types and filenames identify a text file.
//!
//! These are reviewed lists, not content sniffing. Strict UTF-8 decoding stays
//! the safety net for anything they let through.

/// Non-`text/*` media types whose content is text. Any `+json` or `+xml`
/// structured-syntax suffix is text as well.
const TEXT_MEDIA_TYPES: &[&str] = &[
    "application/csv",
    "application/ecmascript",
    "application/graphql",
    "application/javascript",
    "application/json",
    "application/jsonl",
    "application/ndjson",
    "application/sql",
    "application/toml",
    "application/typescript",
    "application/x-csh",
    "application/x-csv",
    "application/x-httpd-php",
    "application/x-javascript",
    "application/x-latex",
    "application/x-ndjson",
    "application/x-perl",
    "application/x-php",
    "application/x-ruby",
    "application/x-sh",
    "application/x-shellscript",
    "application/x-tex",
    "application/x-toml",
    "application/x-typescript",
    "application/x-yaml",
    "application/xml",
    "application/yaml",
];

/// File extensions that name a text file.
const TEXT_EXTENSIONS: &[&str] = &[
    "adoc",
    "bash",
    "bat",
    "c",
    "cc",
    "cfg",
    "cjs",
    "clj",
    "cmake",
    "cmd",
    "conf",
    "cpp",
    "cs",
    "css",
    "csv",
    "cts",
    "dart",
    "diff",
    "dockerfile",
    "env",
    "erl",
    "ex",
    "exs",
    "fish",
    "go",
    "gql",
    "gradle",
    "graphql",
    "groovy",
    "h",
    "hcl",
    "hpp",
    "hs",
    "htm",
    "html",
    "ini",
    "ipynb",
    "java",
    "jl",
    "js",
    "json",
    "jsonl",
    "jsx",
    "kt",
    "kts",
    "less",
    "log",
    "lua",
    "markdown",
    "md",
    "mjs",
    "mk",
    "mts",
    "ndjson",
    "patch",
    "php",
    "pl",
    "pm",
    "properties",
    "proto",
    "ps1",
    "py",
    "r",
    "rb",
    "rs",
    "rst",
    "sass",
    "sbt",
    "scala",
    "scss",
    "sh",
    "sql",
    "svelte",
    "svg",
    "swift",
    "tex",
    "tf",
    "toml",
    "ts",
    "tsv",
    "tsx",
    "txt",
    "vue",
    "xml",
    "yaml",
    "yml",
    "zig",
    "zsh",
];

/// Text files conventionally named without an extension, and dotfiles.
const TEXT_FILENAMES: &[&str] = &[
    ".bashrc",
    ".dockerignore",
    ".editorconfig",
    ".env",
    ".gitattributes",
    ".gitignore",
    ".npmrc",
    ".zshrc",
    "Containerfile",
    "Dockerfile",
    "Gemfile",
    "Jenkinsfile",
    "LICENSE",
    "Makefile",
    "Procfile",
    "README",
    "Rakefile",
    "Vagrantfile",
];

/// Extensions `video/mp2t` is mistaken for: TypeScript sources.
const TYPESCRIPT_EXTENSIONS: &[&str] = &["cts", "mts", "ts"];

fn contains(list: &[&str], value: &str) -> bool {
    list.iter().any(|known| known.eq_ignore_ascii_case(value))
}

fn has_prefix(value: &str, prefix: &str) -> bool {
    value
        .get(..prefix.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(prefix))
}

fn has_suffix(value: &str, suffix: &str) -> bool {
    value
        .len()
        .checked_sub(suffix.len())
        .and_then(|start| value.get(start..))
        .is_some_and(|tail| tail.eq_ignore_ascii_case(suffix))
}

fn is_text_media_type(media_type: &str) -> bool {
    has_prefix(media_type, "text/")
        || has_suffix(media_type, "+json")
        || has_suffix(media_type, "+xml")
        || contains(TEXT_MEDIA_TYPES, media_type)
}

/// The final path component and, unless it is a dotfile, its extension.
fn split_filename(filename: &str) -> (&str, Option<&str>) {
    let basename = filename.rsplit(['/', '\\']).next().unwrap_or(filename);
    // `.gitignore` has no stem, so its whole name is matched instead of an extension.
    let extension = basename
        .rsplit_once('.')
        .filter(|(stem, _)| !stem.is_empty())
        .map(|(_, extension)| extension);
    (basename, extension)
}

/// Whether a filename names a text file by its extension or a known text basename.
pub(crate) fn is_text_filename(filename: &str) -> bool {
    let (basename, extension) = split_filename(filename);
    contains(TEXT_FILENAMES, basename) || extension.is_some_and(|extension| contains(TEXT_EXTENSIONS, extension))
}

/// Image media types an upload accepts, as the recorded `OpenAI` row lists them.
pub(crate) const IMAGE_MEDIA_TYPES: [&str; 5] = ["image/gif", "image/jpeg", "image/jpg", "image/png", "image/webp"];

/// Whether an upload's media type is an accepted image format.
pub(crate) fn is_image_media_type(media_type: &str) -> bool {
    IMAGE_MEDIA_TYPES
        .iter()
        .any(|accepted| media_type.eq_ignore_ascii_case(accepted))
}

/// The media type for an accepted image filename.
pub(crate) fn image_media_type(filename: &str) -> Option<&'static str> {
    let extension = split_filename(filename).1?.to_ascii_lowercase();
    let subtype = if extension == "jpg" { "jpeg" } else { extension.as_str() };
    IMAGE_MEDIA_TYPES
        .iter()
        .find(|media_type| media_type.strip_prefix("image/") == Some(subtype))
        .copied()
}

/// The filename extension shown in a wrong-file-type error, including a leading dot.
pub(crate) fn extension_label(filename: &str) -> String {
    format!(".{}", split_filename(filename).1.unwrap_or_default())
}

/// The formats `OpenAI` lists for context stuffing, used verbatim in a wrong-file-type error.
pub(crate) const CONTEXT_STUFFING_EXTENSIONS: &str = ".art, .bat, .brf, .c, .cls, .css, .csv, .diff, .doc, .docx, .dot, \
     .eml, .epub, .es, .h, .hs, .htm, .html, .hwp, .hwpx, .ics, .ifb, .java, .js, .json, .keynote, .ksh, .ltx, .mail, \
     .markdown, .md, .mht, .mhtml, .mjs, .msg, .nws, .odp, .ods, .odt, .otp, .ots, .pages, .patch, .pdf, .pl, .pm, \
     .pot, .potm, .potx, .ppa, .pps, .ppsm, .ppsx, .ppt, .pptm, .pptx, .pwz, .py, .rst, .rtf, .scala, .sh, .shtml, \
     .srt, .sty, .svg, .svgz, .tex, .text, .txt, .tsv, .vcf, .vtt, .wiz, .xla, .xlb, .xlc, .xlm, .xls, .xlsx, .xlt, \
     .xlw, .xml, .yaml, .yml";

/// Media types a text filename cannot override.
fn is_binary_media_type(media_type: &str, filename: &str) -> bool {
    if media_type.eq_ignore_ascii_case("video/mp2t") {
        return !split_filename(filename)
            .1
            .is_some_and(|extension| contains(TYPESCRIPT_EXTENSIONS, extension));
    }
    media_type.eq_ignore_ascii_case("application/pdf")
        || has_prefix(media_type, "image/")
        || has_prefix(media_type, "audio/")
        || has_prefix(media_type, "video/")
}

/// Whether a file with this media type (empty when absent) and filename is text.
///
/// A text media type is enough on its own. Otherwise a text filename decides,
/// unless the media type is PDF, image, audio, or video.
pub(crate) fn is_text_file(media_type: &str, filename: Option<&str>) -> bool {
    is_text_media_type(media_type)
        || filename.is_some_and(|filename| is_text_filename(filename) && !is_binary_media_type(media_type, filename))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_media_types_need_no_filename() {
        for media_type in [
            "text/plain",
            "TEXT/CSV",
            "application/javascript",
            "application/x-sh",
            "application/toml",
            "application/sql",
            "application/typescript",
            "application/ld+json",
            "application/vnd.api+json",
            "image/svg+xml",
            "APPLICATION/X-NDJSON",
        ] {
            assert!(is_text_file(media_type, None), "{media_type}");
        }
        for media_type in ["", "application/octet-stream", "application/zip", "video/mp2t"] {
            assert!(!is_text_file(media_type, None), "{media_type}");
        }
    }

    #[test]
    fn text_filenames_decide_for_generic_and_unknown_media_types() {
        for name in [
            "main.rb",
            "A.cs",
            "App.KT",
            "build.gradle.kts",
            "src/view.tsx",
            "dir\\script.ps1",
            "schema.graphql",
            "fix.patch",
            "Dockerfile",
            "makefile",
            "repo/.gitignore",
            ".env",
            "CMakeLists.txt",
        ] {
            for media_type in ["", "application/octet-stream", "application/x-unknown"] {
                assert!(is_text_file(media_type, Some(name)), "{media_type} {name}");
            }
        }
        for name in ["archive.zip", "photo", "notes.", ".hidden", "Dockerfile.bin"] {
            assert!(!is_text_file("", Some(name)), "{name}");
        }
    }

    #[test]
    fn binary_media_types_override_text_filenames() {
        for media_type in ["application/pdf", "image/png", "audio/mpeg", "video/mp4", "video/mp2t"] {
            assert!(!is_text_file(media_type, Some("a.txt")), "{media_type}");
        }
        for name in ["index.ts", "lib.mts", "lib.CTS"] {
            assert!(is_text_file("video/mp2t", Some(name)), "{name}");
        }
    }
}
