//! The landing page's video and telemetry sections (issue #1995).

use std::path::{Path, PathBuf};

use xtask_site::{SiteConfig, build};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap()
}

fn landing_html() -> String {
    let site = build(&SiteConfig::from_workspace(&workspace_root())).unwrap();
    site.text("index.html").unwrap().to_string()
}

/// The served page holds a poster that links to the video, and no player: the
/// youtube-nocookie player is created by script only when the poster is
/// clicked, so nothing from YouTube but the thumbnail loads before then.
#[test]
fn the_video_loads_no_player_until_the_visitor_asks() {
    let html = landing_html();
    assert!(
        !html.contains("<iframe"),
        "the served landing page embeds a player"
    );
    let poster = html
        .find("id=\"video-poster\"")
        .map(|at| &html[html[..at].rfind('<').unwrap()..])
        .map(|tag| &tag[..tag.find('>').unwrap()])
        .expect("the landing page lost the video poster");
    assert!(
        poster.starts_with("<a "),
        "the poster is not a link: {poster}"
    );
    assert!(
        poster.contains("href=\"https://www.youtube.com/watch?v=aTB4Iou5YfU\""),
        "without script, the poster no longer opens the video: {poster}"
    );
    assert!(
        poster.contains("data-embed=\"https://www.youtube-nocookie.com/embed/aTB4Iou5YfU"),
        "the player is not the no-cookie one: {poster}"
    );
    assert!(
        poster.contains("aria-label=\"Play the video: "),
        "the poster has no accessible name: {poster}"
    );
    assert!(
        !html.contains("https://www.youtube.com/embed/"),
        "a player from youtube.com, which sets cookies, is on the page"
    );
}

/// Telemetry is the story's fifth step, and its privacy claim is about the
/// deck's own record, not the agents' own telemetry.
#[test]
fn telemetry_is_a_step_of_the_story() {
    let html = landing_html();
    let story_start = html
        .find("<section class=\"story\"")
        .expect("the landing page lost its story");
    let story = &html[story_start..story_start + html[story_start..].find("</section>").unwrap()];
    let step = story
        .find("<span class=\"storyStep\">05</span>")
        .map(|at| &story[at..])
        .expect("the story lost its telemetry step");
    assert!(step.contains("<h3>Know what every run did</h3>"));
    assert!(step.contains("never part of it"));
    assert!(step.contains("The deck’s record is metadata only"));
}
