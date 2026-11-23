use crate::analysis::architecture::{ArchitectureMap, build_architecture_map};
use crate::analysis::dependencies::{DependencyGraph, build_dependency_graph};
use crate::analysis::findings::{Finding, build_findings};
use crate::analysis::import_resolution::{ImportResolutionAnalysis, build_import_resolution};
use crate::analysis::module_index::{ModuleIndex, build_module_index};
use crate::analysis::symbols::{SymbolAnalysis, analyze_repository};
use crate::analysis::test_gaps::{TestGapAnalysis, analyze_test_gaps};
use crate::discovery::{ContributionOpportunity, discover_opportunities};
use crate::github::{GitHubClient, GitHubIssue, GitHubRepository};
use crate::inspection::{RepositoryInfo, inspect_local};
use std::path::Path;

#[derive(Debug)]
pub struct RepositoryAnalysis {
    pub repository: RepositoryInfo,
    pub module_index: ModuleIndex,
    pub dependency_graph: DependencyGraph,
    pub symbols: SymbolAnalysis,
    pub test_gaps: TestGapAnalysis,
    pub imports: ImportResolutionAnalysis,
    pub architecture: ArchitectureMap,
    pub findings: Vec<Finding>,
}

#[derive(Debug)]
pub struct DiscoveryReport {
    pub github_repository: GitHubRepository,
    pub analysis: RepositoryAnalysis,
    pub issues: Vec<GitHubIssue>,
    pub opportunities: Vec<ContributionOpportunity>,
}

pub fn analyze_local_repository(path: &Path) -> Result<RepositoryAnalysis, String> {
    let repository =
        inspect_local(path).map_err(|error| format!("Repository inspection failed: {error}"))?;
    let module_index = build_module_index(&repository);
    let dependency_graph = build_dependency_graph(&repository)?;
    let symbols = analyze_repository(&repository.source_files, &repository.root)?;
    let test_gaps = analyze_test_gaps(&repository.source_files, &repository.root, &symbols);
    let imports = build_import_resolution(&repository, &module_index, &symbols);
    let architecture = build_architecture_map(&repository, &dependency_graph, &symbols);
    let findings = build_findings(&imports, &dependency_graph);
    Ok(RepositoryAnalysis {
        repository,
        module_index,
        dependency_graph,
        symbols,
        test_gaps,
        imports,
        architecture,
        findings,
    })
}

pub fn inspect_github_repository(
    input: &str,
) -> Result<(GitHubRepository, RepositoryAnalysis), String> {
    let client = GitHubClient::new()?;
    let repository = client.repository(input)?;
    let checkout = client.source_checkout(&repository)?;
    let analysis = analyze_local_repository(&checkout)?;
    Ok((repository, analysis))
}

pub fn discover_github_contributions(input: &str) -> Result<DiscoveryReport, String> {
    let client = GitHubClient::new()?;
    let github_repository = client.repository(input)?;
    let checkout = client.source_checkout(&github_repository)?;
    let analysis = analyze_local_repository(&checkout)?;
    let issues = client.open_issues(&github_repository)?;
    let opportunities = discover_opportunities(
        &issues,
        &analysis.repository,
        &analysis.module_index,
        &analysis.symbols,
        &analysis.imports,
        &analysis.architecture,
    );
    Ok(DiscoveryReport {
        github_repository,
        analysis,
        issues,
        opportunities,
    })
}
