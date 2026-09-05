use serde::Serialize;
#[derive(Debug, Serialize)]
pub(crate) struct Check {
    pub id: String,
    pub label: String,
    pub status: String,
    pub message: String,
    pub scope: String,
}
impl Check {
    fn new(id: &str, label: &str, status: &str, message: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            status: status.into(),
            message: message.into(),
            scope: "editing".into(),
        }
    }
    pub fn ok(id: &str, label: &str, message: impl Into<String>) -> Self {
        Self::new(id, label, "ok", message)
    }
    pub fn warn(id: &str, label: &str, message: impl Into<String>) -> Self {
        Self::new(id, label, "warn", message)
    }
    pub fn fail(id: &str, label: &str, message: impl Into<String>) -> Self {
        Self::new(id, label, "fail", message)
    }
}
#[derive(Debug, Serialize)]
pub(crate) struct ProbeReport {
    pub checks: Vec<Check>,
    pub verdict: String,
}
impl Default for ProbeReport {
    fn default() -> Self {
        Self {
            checks: vec![],
            verdict: "ok".into(),
        }
    }
}
impl ProbeReport {
    pub fn add(&mut self, check: Check) {
        if check.status == "fail" || (check.status == "warn" && self.verdict == "ok") {
            self.verdict = check.status.clone();
        }
        self.checks.push(check);
    }
    pub fn print(&self, json: bool) -> anyhow::Result<()> {
        if json {
            println!("{}", serde_json::to_string_pretty(self)?);
        } else {
            for c in &self.checks {
                crate::human!("{:5} {:22} {}", c.status, c.label, c.message);
            }
            crate::human!("Readiness: {}", self.verdict);
        }
        Ok(())
    }
}
#[derive(Debug)]
pub struct ProbeFailed;
impl std::fmt::Display for ProbeFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Handoff readiness checks failed")
    }
}
impl std::error::Error for ProbeFailed {}
