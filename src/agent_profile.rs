use crate::agent::{RunnerMetadata, RunnerSpec};
use crate::domain::{AgentProfile, AgentProfilesConfig, IMPLEMENTER_PROFILE, WorkflowConfig};
use anyhow::{Result, anyhow};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default)]
pub struct ProfileResolveInput {
    pub agent: String,
    pub pi_bin: String,
    pub pi_args: Vec<String>,
    pub config: WorkflowConfig,
    pub profiles: AgentProfilesConfig,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveWorkerProfile {
    pub spec: RunnerSpec,
    pub profile_summary: String,
    pub launch_summary: String,
    pub source_attribution: BTreeMap<String, String>,
}

pub fn resolve_effective_worker_profile(
    input: ProfileResolveInput,
) -> Result<EffectiveWorkerProfile> {
    let (agent, agent_source) = choose_text(
        input.agent.trim(),
        "request",
        input.config.agent.trim(),
        "workflow_config",
        "pi",
        "builtin",
    );
    let agent = agent.to_ascii_lowercase();
    let mut source_attribution = BTreeMap::new();
    source_attribution.insert("agent".to_string(), agent_source.to_string());

    if agent == "fake" {
        let mut spec = RunnerSpec::from_parts("fake", String::new(), Vec::new())?;
        let metadata = RunnerMetadata {
            profile_summary: "fake runner".to_string(),
            launch_summary: "fake runner".to_string(),
            source_attribution: source_attribution.clone(),
            ..RunnerMetadata::default()
        };
        spec.metadata = metadata;
        return Ok(EffectiveWorkerProfile {
            spec,
            profile_summary: "fake runner".to_string(),
            launch_summary: "fake runner".to_string(),
            source_attribution,
        });
    }
    if agent != "pi" {
        return Err(anyhow!(
            "unknown agent {agent:?}; expected \"pi\" or \"fake\""
        ));
    }

    let (pi_bin, pi_bin_source) =
        choose_text(input.pi_bin.trim(), "request", "", "", "pi", "builtin");
    source_attribution.insert("pi_bin".to_string(), pi_bin_source.to_string());
    source_attribution.insert(
        "pi_args".to_string(),
        if input.pi_args.is_empty() {
            "none"
        } else {
            "request"
        }
        .to_string(),
    );

    let profile = input
        .profiles
        .profiles
        .get(IMPLEMENTER_PROFILE)
        .ok_or_else(|| anyhow!("missing required agent profile {IMPLEMENTER_PROFILE:?}"))?;
    profile.validate_required(IMPLEMENTER_PROFILE)?;
    source_attribution.insert("profile".to_string(), "resolved_agent_profile".to_string());
    source_attribution.insert("provider".to_string(), "resolved_agent_profile".to_string());
    source_attribution.insert("model".to_string(), "resolved_agent_profile".to_string());
    source_attribution.insert(
        "reasoning".to_string(),
        "resolved_agent_profile".to_string(),
    );
    source_attribution.insert("mode".to_string(), "resolved_agent_profile".to_string());

    let mut pi_args = pi_profile_args(profile);
    pi_args.extend(input.pi_args.iter().cloned());

    let profile_summary = format!(
        "{}: provider={} model={} reasoning={} mode={}",
        IMPLEMENTER_PROFILE,
        profile.provider.trim(),
        profile.model.trim(),
        profile.reasoning.trim(),
        profile.mode.trim()
    );
    let launch_summary = format!("pi {profile_summary}");
    let metadata = RunnerMetadata {
        profile: IMPLEMENTER_PROFILE.to_string(),
        provider: profile.provider.trim().to_string(),
        model: profile.model.trim().to_string(),
        reasoning: profile.reasoning.trim().to_string(),
        mode: profile.mode.trim().to_string(),
        profile_summary: profile_summary.clone(),
        launch_summary: launch_summary.clone(),
        fix_commands: vec!["pi /login".to_string()],
        source_attribution: source_attribution.clone(),
    };
    let mut spec = RunnerSpec::from_parts("pi", pi_bin.to_string(), pi_args)?;
    spec.metadata = metadata;

    Ok(EffectiveWorkerProfile {
        spec,
        profile_summary,
        launch_summary,
        source_attribution,
    })
}

fn choose_text<'a>(
    primary: &'a str,
    primary_source: &'static str,
    secondary: &'a str,
    secondary_source: &'static str,
    fallback: &'a str,
    fallback_source: &'static str,
) -> (&'a str, &'static str) {
    if !primary.trim().is_empty() {
        (primary.trim(), primary_source)
    } else if !secondary.trim().is_empty() {
        (secondary.trim(), secondary_source)
    } else {
        (fallback, fallback_source)
    }
}

fn pi_profile_args(profile: &AgentProfile) -> Vec<String> {
    let mut args = vec![
        "--provider".to_string(),
        profile.provider.trim().to_string(),
        "--model".to_string(),
        profile.model.trim().to_string(),
        "--thinking".to_string(),
        profile.reasoning.trim().to_string(),
    ];
    args.extend(profile.args.iter().cloned());
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_bypasses_pi_profile() {
        let effective = resolve_effective_worker_profile(ProfileResolveInput {
            agent: "fake".to_string(),
            profiles: AgentProfilesConfig::default(),
            config: WorkflowConfig::default(),
            ..ProfileResolveInput::default()
        })
        .unwrap();
        assert_eq!(effective.spec.kind, "fake");
        assert!(effective.spec.pi_args.is_empty());
        assert_eq!(effective.spec.metadata.launch_summary(), "fake runner");
    }

    #[test]
    fn pi_profile_generates_args_then_request_overrides_last() {
        let mut profiles = AgentProfilesConfig::default();
        profiles.profiles.insert(
            IMPLEMENTER_PROFILE.to_string(),
            AgentProfile {
                provider: "openai".to_string(),
                model: "gpt-5.5".to_string(),
                reasoning: "xhigh".to_string(),
                mode: "fast".to_string(),
                args: vec!["--some-profile-arg".to_string()],
                required: true,
                read_only: false,
            },
        );
        let effective = resolve_effective_worker_profile(ProfileResolveInput {
            agent: "".to_string(),
            pi_bin: "custom-pi".to_string(),
            pi_args: vec!["--model".to_string(), "override".to_string()],
            profiles,
            config: WorkflowConfig::default(),
        })
        .unwrap();
        assert_eq!(effective.spec.kind, "pi");
        assert_eq!(effective.spec.pi_bin, "custom-pi");
        assert_eq!(
            effective.spec.pi_args,
            vec![
                "--provider",
                "openai",
                "--model",
                "gpt-5.5",
                "--thinking",
                "xhigh",
                "--some-profile-arg",
                "--model",
                "override",
            ]
        );
        assert!(effective.launch_summary.contains("provider=openai"));
        assert_eq!(
            effective
                .spec
                .metadata
                .source_attribution
                .get("pi_args")
                .unwrap(),
            "request"
        );
    }

    #[test]
    fn operator_profiles_override_stale_repo_profiles() {
        let mut repo_profiles = AgentProfilesConfig::default();
        repo_profiles.profiles.insert(
            IMPLEMENTER_PROFILE.to_string(),
            AgentProfile {
                provider: "openai".to_string(),
                model: "gpt-5.5".to_string(),
                reasoning: "xhigh".to_string(),
                mode: "fast".to_string(),
                required: true,
                read_only: false,
                ..AgentProfile::default()
            },
        );
        let profiles = repo_profiles.with_operator_overrides(AgentProfilesConfig::default());

        let effective = resolve_effective_worker_profile(ProfileResolveInput {
            profiles,
            config: WorkflowConfig::default(),
            ..ProfileResolveInput::default()
        })
        .unwrap();

        assert_eq!(effective.spec.pi_args[0..2], ["--provider", "openai-codex"]);
        assert!(effective.launch_summary.contains("provider=openai-codex"));
    }
}
