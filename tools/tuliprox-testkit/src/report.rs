use crate::TestkitError;
use serde::Serialize;
use std::{fs, path::Path};

#[derive(Debug, Serialize, serde::Deserialize)]
pub struct RunReport<'a> {
    pub schema_version: u16,
    pub scenario: &'a str,
    pub outcome: &'a str,
    pub policy_hash: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_id: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub events: Vec<ReportEvent<'a>>,
}

#[derive(Debug, Serialize, serde::Deserialize)]
pub struct ReportEvent<'a> {
    pub source: &'a str,
    pub playback_id: &'a str,
    pub event: &'a str,
    pub detail: &'a str,
}

impl RunReport<'_> {
    pub fn write_to(&self, directory: &Path) -> Result<(), TestkitError> {
        fs::create_dir_all(directory)?;
        let json = serde_json::to_vec_pretty(self)
            .map_err(|error| TestkitError::Protocol(format!("cannot serialize report: {error}")))?;
        fs::write(directory.join("run.json"), json)?;
        fs::write(directory.join("events.ndjson"), self.events_ndjson()?)?;
        let run_id_line = self.run_id.map_or_else(String::new, |id| format!("run_id: {id}\n"));
        let summary = format!(
            "scenario: {}\noutcome: {}\npolicy: {}\n{}events: {}\n",
            self.scenario,
            self.outcome,
            self.policy_hash,
            run_id_line,
            self.events.len()
        );
        fs::write(directory.join("summary.txt"), summary)?;
        fs::write(directory.join("junit.xml"), self.junit_xml())?;
        Ok(())
    }

    fn junit_xml(&self) -> String {
        let failed = usize::from(self.outcome != "passed");
        let name = xml_escape(self.scenario);
        if failed == 0 {
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"tuliprox-testkit\" tests=\"1\" failures=\"0\"><testcase name=\"{name}\"/></testsuite>\n"
            )
        } else {
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<testsuite name=\"tuliprox-testkit\" tests=\"1\" failures=\"{failed}\"><testcase name=\"{name}\"><failure message=\"{}\"/></testcase></testsuite>\n",
                xml_escape(self.outcome)
            )
        }
    }

    fn events_ndjson(&self) -> Result<String, TestkitError> {
        let mut lines = String::new();
        for event in &self.events {
            let encoded = serde_json::to_string(event)
                .map_err(|error| TestkitError::Protocol(format!("cannot serialize report event: {error}")))?;
            lines.push_str(&encoded);
            lines.push('\n');
        }
        Ok(lines)
    }
}

fn xml_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn junit_marks_failed_runs() {
        let report = RunReport {
            schema_version: 1,
            scenario: "a&b",
            outcome: "failed",
            policy_hash: "hash",
            run_id: None,
            error_kind: None,
            error_message: None,
            exit_code: None,
            events: Vec::new(),
        };
        assert!(report.junit_xml().contains("failures=\"1\""));
        assert!(report.junit_xml().contains("a&amp;b"));
    }

    #[test]
    fn events_are_emitted_as_one_json_object_per_line() {
        let report = RunReport {
            schema_version: 1,
            scenario: "scenario",
            outcome: "passed",
            policy_hash: "hash",
            run_id: None,
            error_kind: None,
            error_message: None,
            exit_code: None,
            events: vec![ReportEvent {
                source: "controller",
                playback_id: "playback-a",
                event: "started",
                detail: "ok",
            }],
        };
        let lines = report.events_ndjson().unwrap();
        assert_eq!(lines.lines().count(), 1);
        assert_eq!(serde_json::from_str::<serde_json::Value>(lines.trim()).unwrap()["playback_id"], "playback-a");
    }

    #[test]
    fn report_serializes_and_deserializes_with_run_id_and_error() {
        let report = RunReport {
            schema_version: 1,
            scenario: "scenario_fail",
            outcome: "failed",
            policy_hash: "pre-start",
            run_id: Some("test-run-123"),
            error_kind: Some("bootstrap_error"),
            error_message: Some("address already in use"),
            exit_code: Some(4),
            events: Vec::new(),
        };
        let json = serde_json::to_string(&report).unwrap();
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(value["run_id"], "test-run-123");
        assert_eq!(value["error_kind"], "bootstrap_error");
        assert_eq!(value["error_message"], "address already in use");
        assert_eq!(value["exit_code"], 4);
    }
}
