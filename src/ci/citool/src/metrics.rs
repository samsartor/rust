use std::path::Path;

use crate::jobs::JobDatabase;
use anyhow::Context;
use build_helper::metrics::{JsonNode, JsonRoot, TestSuite};
use std::collections::HashMap;

pub type JobName = String;

pub fn get_test_suites(metrics: &JsonRoot) -> Vec<&TestSuite> {
    fn visit_test_suites<'a>(nodes: &'a [JsonNode], suites: &mut Vec<&'a TestSuite>) {
        for node in nodes {
            match node {
                JsonNode::RustbuildStep { children, .. } => {
                    visit_test_suites(&children, suites);
                }
                JsonNode::TestSuite(suite) => {
                    suites.push(&suite);
                }
            }
        }
    }

    let mut suites = vec![];
    for invocation in &metrics.invocations {
        visit_test_suites(&invocation.children, &mut suites);
    }
    suites
}

pub fn load_metrics(path: &Path) -> anyhow::Result<JsonRoot> {
    let metrics = std::fs::read_to_string(path)
        .with_context(|| format!("Cannot read JSON metrics from {path:?}"))?;
    let metrics: JsonRoot = serde_json::from_str(&metrics)
        .with_context(|| format!("Cannot deserialize JSON metrics from {path:?}"))?;
    Ok(metrics)
}

pub struct JobMetrics {
    pub parent: Option<JsonRoot>,
    pub current: JsonRoot,
}

/// Download before/after metrics for all auto jobs in the job database.
/// `parent` and `current` should be commit SHAs.
pub fn download_auto_job_metrics(
    job_db: &JobDatabase,
    parent: &str,
    current: &str,
) -> anyhow::Result<HashMap<JobName, JobMetrics>> {
    let mut jobs = HashMap::default();

    for job in &job_db.auto_jobs {
        eprintln!("Downloading metrics of job {}", job.name);
        let metrics_parent = match download_job_metrics(&job.name, parent) {
            Ok(metrics) => Some(metrics),
            Err(error) => {
                eprintln!(
                    r#"Did not find metrics for job `{}` at `{}`: {error:?}.
Maybe it was newly added?"#,
                    job.name, parent
                );
                None
            }
        };
        let metrics_current = download_job_metrics(&job.name, current)?;
        jobs.insert(
            job.name.clone(),
            JobMetrics { parent: metrics_parent, current: metrics_current },
        );
    }
    Ok(jobs)
}

pub fn download_job_metrics(job_name: &str, sha: &str) -> anyhow::Result<JsonRoot> {
    let url = get_metrics_url(job_name, sha);
    let mut response = ureq::get(&url).call()?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "Cannot fetch metrics from {url}: {}\n{}",
            response.status(),
            response.body_mut().read_to_string()?
        ));
    }
    let data: JsonRoot = response
        .body_mut()
        .read_json()
        .with_context(|| anyhow::anyhow!("cannot deserialize metrics from {url}"))?;
    Ok(data)
}

fn get_metrics_url(job_name: &str, sha: &str) -> String {
    let suffix = if job_name.ends_with("-alt") { "-alt" } else { "" };
    format!("https://ci-artifacts.rust-lang.org/rustc-builds{suffix}/{sha}/metrics-{job_name}.json")
}
