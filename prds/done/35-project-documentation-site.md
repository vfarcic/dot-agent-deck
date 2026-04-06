# PRD #35: Project Documentation Site

**Status**: Complete
**Completed**: 2026-04-06
**Priority**: Low
**Created**: 2026-04-03
**GitHub Issue**: [#35](https://github.com/vfarcic/dot-agent-deck/issues/35)

## Problem Statement

The project lacks a dedicated site for documentation, guides, and feature overview. Everything currently lives in the README, which becomes harder to navigate as the project grows. New users have no polished entry point to understand what dot-agent-deck does or how to get started.

## Solution Overview

Create a documentation site generated from a `docs/` directory in this repo and deployed to a Kubernetes cluster. The site should cover installation, usage guides, configuration, and feature overview.

## Scope

### In Scope
- `docs/` directory in this repo as the content source
- Static site generation (tool TBD — e.g., mdBook, Hugo, Astro)
- Deployment to a Kubernetes cluster
- Core content: installation, usage, configuration, keybindings, feature overview

### Out of Scope
- Blog / news section (future enhancement)
- User accounts or interactive features
- Custom domain setup (can be added later)

## Technical Approach

- **Content source**: `docs/` directory in this repository
- **Site generator**: Docusaurus v3 (chosen for unified look across custom homepage and docs)
- **Hosting**: Kubernetes cluster via Helm chart, monitored by Argo CD
- **Container**: Multi-stage Docker build (Node.js builder + nginx), published to ghcr.io
- **CI/CD**: Docs image built and chart updated as part of release workflow (on `v*` tags)
- **URL**: https://agent-deck.devopstoolkit.ai

## Success Criteria

- Site is accessible and serves documentation from the `docs/` directory
- Content covers the core user journey (install, configure, use)
- Site rebuilds and redeploys automatically when docs change
- Runs on a Kubernetes cluster

## Milestones

- [x] `docs/` directory created with initial content structure
- [x] Static site generator chosen and configured (Docusaurus v3)
- [x] Core documentation pages written (install, usage, configuration, keybindings)
- [x] Kubernetes deployment manifests created (Helm chart + Dockerfile)
- [x] CI/CD pipeline for automated build and deploy on docs changes
- [ ] Site live and accessible

## Key Files

- `docs/` — Documentation content source (7 Markdown files)
- `site/` — Docusaurus v3 project with custom homepage
- `site/docusaurus.config.js` — Site configuration (ingests `../docs`)
- `site/Dockerfile` — Multi-stage Docker build (Node.js + nginx)
- `site/helm/` — Helm chart for Kubernetes deployment
- `.github/workflows/docs.yml` — CI/CD pipeline: build, push image, update chart

## Risks

- **Generator choice**: Picking the wrong tool could require migration later. Mitigate by evaluating a few options before committing.
- **Content maintenance**: Docs can drift from code. Mitigate by including doc updates in feature PRD milestones.
