//! Shared media helpers for provider adapters that return generated images.
//!
//! Providers hand back image bytes as base64 `data:` URLs (or bare base64) and
//! tag them with a content type / output format. The mapping between MIME types
//! and file extensions, and the parsing of `data:<mime>;base64,<payload>` URLs,
//! is identical across adapters, so it lives here. Each adapter keeps only its
//! own wire-shape extraction (where the base64 lives in the response) and its own
//! fallback/error policy.

/// Map an image content type to the file extension used when naming a generated
/// artifact. Only the formats the providers actually emit are special-cased;
/// everything else falls back to `png`.
pub fn extension_for_content_type(content_type: &str) -> &'static str {
    match content_type.trim().to_ascii_lowercase().as_str() {
        "image/jpeg" => "jpg",
        "image/webp" => "webp",
        _ => "png",
    }
}

/// Map an `output_format` hint (as returned by the OpenAI/Codex Responses image
/// tool) to a content type. The inverse partner of
/// [`extension_for_content_type`]; unknown formats fall back to `image/png`.
pub fn content_type_for_format(format: &str) -> &'static str {
    match format.trim().to_ascii_lowercase().as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        _ => "image/png",
    }
}

/// Parse a base64 image value of the form `data:<mime>;base64,<payload>`.
///
/// Returns `Some((mime, payload))` when the value is a well-formed base64 data
/// URL. Returns `None` for anything else (bare base64, a non-base64 URL, …) so
/// each adapter can apply its own policy: Codex falls back to a content type
/// derived from the model's `output_format`, while OpenRouter treats a missing
/// data URL as an error.
pub fn split_base64_data_url(value: &str) -> Option<(String, String)> {
    let rest = value.trim().strip_prefix("data:")?;
    let (mime, data) = rest.split_once(";base64,")?;
    Some((mime.to_string(), data.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_for_content_type_maps_known_image_types() {
        assert_eq!(extension_for_content_type("image/jpeg"), "jpg");
        assert_eq!(extension_for_content_type("image/webp"), "webp");
        assert_eq!(extension_for_content_type("image/png"), "png");
        assert_eq!(extension_for_content_type("IMAGE/JPEG "), "jpg");
        assert_eq!(
            extension_for_content_type("application/octet-stream"),
            "png"
        );
    }

    #[test]
    fn content_type_for_format_maps_known_formats() {
        assert_eq!(content_type_for_format("jpg"), "image/jpeg");
        assert_eq!(content_type_for_format("jpeg"), "image/jpeg");
        assert_eq!(content_type_for_format("webp"), "image/webp");
        assert_eq!(content_type_for_format("PNG"), "image/png");
        assert_eq!(content_type_for_format("gif"), "image/png");
    }

    #[test]
    fn split_base64_data_url_extracts_mime_and_payload() {
        assert_eq!(
            split_base64_data_url("data:image/png;base64,Zm9v"),
            Some(("image/png".to_string(), "Zm9v".to_string()))
        );
        assert_eq!(
            split_base64_data_url("  data:image/webp;base64,YmFy  "),
            Some(("image/webp".to_string(), "YmFy".to_string()))
        );
    }

    #[test]
    fn split_base64_data_url_rejects_non_data_urls() {
        assert_eq!(split_base64_data_url("Zm9v"), None);
        assert_eq!(split_base64_data_url("https://example.com/a.png"), None);
        assert_eq!(split_base64_data_url("data:image/png,Zm9v"), None);
    }
}
