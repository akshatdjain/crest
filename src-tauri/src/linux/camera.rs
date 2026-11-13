//! The Camera tool on Linux. The page shows the camera with the browser's own `getUserMedia` (the same on every platform), so all
//! the backend does is let the webview do it: WebKitGTK ships with media capture off and asks a permission handler for everything
//! else, so it is switched on and the handler answers for Crest's own page and for the camera only.

use tauri::WebviewWindow;
use webkit2gtk::glib::prelude::Cast;
use webkit2gtk::{PermissionRequestExt, SettingsExt, UserMediaPermissionRequest, UserMediaPermissionRequestExt, WebViewExt};

/// Whether `uri` is a page of Crest itself: the app's own scheme, or the dev server while developing.
fn own_page(uri: &str) -> bool {
    uri.starts_with("tauri://") || uri.starts_with("http://tauri.localhost") || uri.starts_with("http://localhost:1420")
}

/// Allows a video-only media request from Crest's own page; the microphone and anything from another page is refused.
fn allowed(uri: &str, video: bool, audio: bool) -> bool {
    own_page(uri) && video && !audio
}

/// Called once, after the window exists.
pub fn install(window: &WebviewWindow) {
    let _ = window.with_webview(|webview| {
        let view = webview.inner();
        if let Some(settings) = WebViewExt::settings(&view) {
            settings.set_enable_media_stream(true);
        }
        view.connect_permission_request(|view, request| {
            let Some(media) = request.downcast_ref::<UserMediaPermissionRequest>() else { return false };
            let uri = view.uri().map(|u| u.to_string()).unwrap_or_default();
            if allowed(&uri, media.is_for_video_device(), media.is_for_audio_device()) {
                request.allow();
            } else {
                request.deny();
            }
            true
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_camera_for_crests_own_page_is_allowed() {
        assert!(allowed("tauri://localhost/index.html", true, false));
        assert!(allowed("http://localhost:1420/", true, false));
        assert!(!allowed("tauri://localhost/", true, true), "a microphone too");
        assert!(!allowed("tauri://localhost/", false, true), "the microphone alone");
        assert!(!allowed("https://example.com/", true, false), "another page");
        assert!(!allowed("http://localhost:8080/", true, false), "another local server");
        assert!(!allowed("", true, false));
    }
}
