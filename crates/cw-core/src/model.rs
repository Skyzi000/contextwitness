use serde::{Deserialize, Serialize};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrStatus {
    Succeeded,
    NoText,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenPayload {
    pub monitor_id: String,
    pub width: u32,
    pub height: u32,
    pub image_path: Option<String>,
    pub ocr_status: OcrStatus,
    pub ocr_error: Option<String>,
    pub ocr_text: Option<String>,
    pub ocr_langs: Vec<String>,
    pub foreground_process: Option<String>,
    pub foreground_window_title: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SourcePayload {
    Screen(ScreenPayload),
    /// A row whose source this build does not know is kept as it was read, so a window holding
    /// one still builds its episode instead of failing the whole read.
    Unknown {
        source: String,
        raw: serde_json::Value,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub id: ulid::Ulid,
    pub observed_at: chrono::DateTime<chrono::Utc>,
    pub duration_ms: Option<u64>,
    pub schema_version: u32,
    pub payload: SourcePayload,
}

impl SourcePayload {
    /// "screen" for Screen; the stored source string for Unknown.
    pub fn kind(&self) -> &str {
        match self {
            Self::Screen(_) => "screen",
            Self::Unknown { source, .. } => source,
        }
    }

    /// Payload-only JSON (no discriminator inside). Unknown returns its raw value unchanged.
    pub fn to_payload_json(&self) -> Result<serde_json::Value, serde_json::Error> {
        match self {
            Self::Screen(payload) => Ok(serde_json::to_value(payload)?),
            Self::Unknown { raw, .. } => Ok(raw.clone()),
        }
    }

    /// Inverse: "screen" parses ScreenPayload (unknown JSON fields are ignored for forward
    /// compatibility); any other source becomes Unknown{source, raw} without validation.
    pub fn from_parts(
        source: &str,
        raw: serde_json::Value,
    ) -> Result<SourcePayload, serde_json::Error> {
        if source == "screen" {
            Ok(Self::Screen(serde_json::from_value(raw)?))
        } else {
            Ok(Self::Unknown {
                source: source.to_owned(),
                raw,
            })
        }
    }
}

impl Observation {
    /// id = new ULID, schema_version = CURRENT_SCHEMA_VERSION, duration_ms = None.
    pub fn new_screen(
        payload: ScreenPayload,
        observed_at: chrono::DateTime<chrono::Utc>,
    ) -> Observation {
        Self {
            id: ulid::Ulid::new(),
            observed_at,
            duration_ms: None,
            schema_version: CURRENT_SCHEMA_VERSION,
            payload: SourcePayload::Screen(payload),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fully_populated_screen_payload() -> ScreenPayload {
        ScreenPayload {
            monitor_id: "monitor-1".to_owned(),
            width: 1920,
            height: 1080,
            image_path: Some("screens/observation.png".to_owned()),
            ocr_status: OcrStatus::Succeeded,
            ocr_error: Some("non-fatal OCR warning".to_owned()),
            ocr_text: Some("テスト".to_owned()),
            ocr_langs: vec!["ja-JP".to_owned(), "en-US".to_owned()],
            foreground_process: Some("notepad.exe".to_owned()),
            foreground_window_title: Some("メモ帳".to_owned()),
        }
    }

    #[test]
    fn screen_payload_roundtrips_via_parts() {
        let payload = fully_populated_screen_payload();
        let observed_at = chrono::DateTime::parse_from_rfc3339("2026-07-25T12:34:56Z")
            .expect("test timestamp should be valid")
            .with_timezone(&chrono::Utc);
        let obs = Observation::new_screen(payload, observed_at);

        let json = obs
            .payload
            .to_payload_json()
            .expect("screen payload should serialize");
        let back = SourcePayload::from_parts(obs.payload.kind(), json)
            .expect("screen payload should deserialize");

        assert_eq!(back, obs.payload);
        assert_eq!(obs.schema_version, 1);
    }

    #[test]
    fn payload_json_matches_golden_fixture() {
        let payload = fully_populated_screen_payload();
        // Spelled through `to_payload_json`, the conversion every stored row goes through: the
        // struct's own Serialize orders keys as declared, which the database never sees —
        // serde_json's maps sort their keys.
        let json = serde_json::to_string(
            &SourcePayload::Screen(payload)
                .to_payload_json()
                .expect("screen payload should serialize"),
        )
        .expect("payload JSON should spell as a string");
        let golden = r#"{"foreground_process":"notepad.exe","foreground_window_title":"メモ帳","height":1080,"image_path":"screens/observation.png","monitor_id":"monitor-1","ocr_error":"non-fatal OCR warning","ocr_langs":["ja-JP","en-US"],"ocr_status":"succeeded","ocr_text":"テスト","width":1920}"#;

        assert_eq!(json, golden);
        assert!(
            serde_json::from_str::<serde_json::Value>(&json)
                .expect("golden payload should be valid JSON")
                .get("source")
                .is_none()
        );
    }

    #[test]
    fn unknown_source_roundtrips_untouched() {
        let raw = serde_json::json!({
            "codec": "pcm_s16le",
            "channels": 2,
            "metadata": {
                "labels": ["meeting", "voice"],
                "nullable": null
            }
        });

        let payload =
            SourcePayload::from_parts("audio", raw.clone()).expect("unknown source should be kept");

        assert_eq!(
            payload,
            SourcePayload::Unknown {
                source: "audio".to_owned(),
                raw: raw.clone(),
            }
        );
        assert_eq!(payload.kind(), "audio");
        assert_eq!(
            payload
                .to_payload_json()
                .expect("unknown payload should be returned"),
            raw
        );
    }

    #[test]
    fn screen_parse_ignores_unknown_fields() {
        let mut raw = serde_json::to_value(fully_populated_screen_payload())
            .expect("payload should serialize");
        raw.as_object_mut()
            .expect("screen payload should be an object")
            .insert("future_field".to_owned(), serde_json::json!(1));

        let parsed =
            SourcePayload::from_parts("screen", raw).expect("future fields should be ignored");

        assert!(matches!(parsed, SourcePayload::Screen(_)));
    }
}
