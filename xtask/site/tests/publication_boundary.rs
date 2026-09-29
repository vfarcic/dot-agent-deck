use std::fs;
use std::path::{Path, PathBuf};

use xtask_site::{SiteConfig, build, published_docs};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn develop_markdown(docs: &Path) -> Vec<(String, Vec<u8>)> {
    fn visit(dir: &Path, root: &Path, pages: &mut Vec<(String, Vec<u8>)>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                visit(&path, root, pages);
            } else if path.extension().is_some_and(|extension| extension == "md") {
                pages.push((
                    path.strip_prefix(root).unwrap().display().to_string(),
                    fs::read(&path).unwrap(),
                ));
            }
        }
    }
    let mut pages = Vec::new();
    visit(&docs.join("develop"), docs, &mut pages);
    assert!(
        !pages.is_empty(),
        "expected maintainer pages under docs/develop/"
    );
    pages
}

fn contains_bytes(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Scenario: Build the real site and compare every generated file with each
/// maintainer Markdown page. No full maintainer page may enter the site or the
/// two LLM indexes, even when it is published under a harmless-looking name.
#[test]
fn site_excludes_develop_page_content() {
    let config = SiteConfig::from_workspace(&workspace_root());
    let site = build(&config).unwrap();
    for (source, body) in develop_markdown(&config.docs_dir) {
        for (output, bytes) in &site.files {
            assert!(
                !contains_bytes(bytes, &body),
                "{output} contains all bytes of {source}"
            );
        }
    }
}

/// Scenario: Parse manifest entries that try to spell a maintainer path with
/// dot segments, backslashes or different letter case. Each entry must fail
/// before the site or binary can use it.
#[test]
fn manifest_rejects_alternate_develop_spellings() {
    for slug in [
        "./develop/secret",
        "other/../develop/secret",
        "develop\\secret",
        "Develop/secret",
        "DEVELOP/secret",
        "develop/../secret",
    ] {
        let manifest =
            format!("[[page]]\nslug = {slug:?}\ntitle = \"Secret\"\ndescription = \"No\"\n");
        assert!(
            published_docs::parse(&manifest).is_err(),
            "accepted alternate maintainer slug {slug:?}"
        );
    }
}

/// Scenario: A published page aliases a maintainer page through a symlink
/// inside docs. The site build must reject that alias rather than copying the
/// private content under the published slug.
#[cfg(unix)]
#[test]
fn site_rejects_symlinked_develop_page() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let docs = root.path().join("docs");
    fs::create_dir_all(docs.join("develop")).unwrap();
    fs::create_dir_all(docs.join("img")).unwrap();
    fs::write(
        docs.join("develop/secret.md"),
        "# Secret\nprivate sentinel\n",
    )
    .unwrap();
    symlink("develop/secret.md", docs.join("alias.md")).unwrap();
    fs::write(
        docs.join("published.toml"),
        "[[page]]\nslug = \"alias\"\ntitle = \"Secret\"\ndescription = \"Alias\"\n",
    )
    .unwrap();
    let config = SiteConfig {
        docs_dir: docs,
        base_url: "https://example.test".to_string(),
    };
    match build(&config) {
        Err(error) => assert!(
            error.contains("develop") || error.contains("symlink"),
            "{error}"
        ),
        Ok(site) => panic!(
            "symlink into docs/develop/ was published as docs/alias.md: {}",
            site.text("docs/alias.md").unwrap_or("<missing>")
        ),
    }
}

/// Scenario: A published page links to a maintainer Markdown page with a
/// relative URL. The site build must reject the page instead of shipping a
/// link to material outside the published manifest.
#[test]
fn site_rejects_links_into_develop() {
    let root = tempfile::tempdir().unwrap();
    let docs = root.path().join("docs");
    fs::create_dir_all(docs.join("develop")).unwrap();
    fs::create_dir_all(docs.join("img")).unwrap();
    fs::write(
        docs.join("develop/secret.md"),
        "# Secret\nprivate sentinel\n",
    )
    .unwrap();
    fs::write(
        docs.join("start.md"),
        "# Start\n[private](develop/secret.md)\n",
    )
    .unwrap();
    fs::write(
        docs.join("published.toml"),
        "[[page]]\nslug = \"start\"\ntitle = \"Start\"\ndescription = \"Start\"\n",
    )
    .unwrap();
    let config = SiteConfig {
        docs_dir: docs,
        base_url: "https://example.test".to_string(),
    };
    match build(&config) {
        Err(error) => assert!(
            error.contains("develop") || error.contains("unpublished"),
            "{error}"
        ),
        Ok(site) => panic!(
            "link into docs/develop/ was published in docs/start.md: {}",
            site.text("docs/start.md").unwrap_or("<missing>")
        ),
    }
}
