const TRAPI_BASE: &str = "https://trapi.research.microsoft.com/redmond/interactive";

#[test]
fn preserves_trapi_base_path() {
	let upstream = AzureUpstreamRef::resolve("gpt-4o-mini_2024-07-18", Some(TRAPI_BASE))
		.expect("TRAPI base path must be accepted");
	assert!(upstream.chat_completions_url().starts_with(&format!(
		"{TRAPI_BASE}/openai/deployments/gpt-4o-mini_2024-07-18/chat/completions?"
	)));
}

#[test]
fn accepts_trapi_dated_deployment_id_verbatim() {
	let deployment = "gpt-5.2_2025-12-11";
	let upstream = AzureUpstreamRef::resolve(deployment, Some(TRAPI_BASE))
		.expect("Existing TRAPI deployment IDs must remain usable");
	assert_eq!(upstream.deployment, deployment);
}

#[test]
fn uses_trapi_generation_api_version() {
	let upstream = AzureUpstreamRef::resolve("gpt-4o-mini_2024-07-18", Some(TRAPI_BASE))
		.expect("Resolve a deployment without dots to isolate the version mismatch");
	assert_eq!(upstream.api_version, "2025-04-01-preview");
}

#[test]
fn accepts_explicit_trapi_version_in_base_url() {
	let base = format!("{TRAPI_BASE}?api-version=2025-04-01-preview");
	let upstream = AzureUpstreamRef::resolve("gpt-4o-mini_2024-07-18", Some(&base))
		.expect("An api_base query should allow pinning TRAPI's version");
	assert_eq!(
		upstream.chat_completions_url(),
		format!(
			"{TRAPI_BASE}/openai/deployments/gpt-4o-mini_2024-07-18/chat/completions?api-version=2025-04-01-preview"
		)
	);
}

// This checks a source constant, not a running SDK or minted access token.
#[test]
fn source_token_audience_matches_trapi() {
	assert_eq!(AZURE_OPENAI_SCOPE, "api://trapi/.default");
}
