
# Focus of this release                                    <!-- :TOC: -->
  - [Drop usage of meta `rust-build.yml` workflows](#drop-usage-of-meta-rust-buildyml-workflows)
  - [Allow specifying precise workflow version](#allow-specifying-precise-workflow-version)
  - [Workflow Runtime information is done in fslabscli](#workflow-runtime-information-is-done-in-fslabscli)
  - [Installer publishing need are now separate from their binary](#installer-publishing-need-are-now-separate-from-their-binary)
  - [Nightly version](#nightly-version)
- [Missing from this release](#missing-from-this-release)
  - [Release notification and tagging](#release-notification-and-tagging)

## Drop usage of meta `rust-build.yml` workflows

Back when we switch our ci usage to use reusable workflows we set up a meta workflows: rust-build.yaml. The goal was to have a single build entry to control the release mechanism. Based on it&rsquo;s inputs, the workflow would conditionnaly enables the correct publishing sub workflows.

This approach, while solving a lot of our issues and helping bring more stability to the ci pipelines cam with two main caveats:

  1.  __CI Workflows referencing__: Github reusable workflow version needs to be known before running. But as we are referencing only the meta workflows, we have no ways of forwarding that workflow version to the sub workflows.
  2.  __Github reusable workflows limit__: For [fsl_libs](https://github.com/ForesightMiningSoftwareCorporation/fsl_libs), a publishing workflow might publish about 50 packages, this means that the compiled workflows (the full workflow with reusable workflows inlined) would be about 50 \* (1 + 5) workflow long so, those 300 workflows were hitting the undocumented size limit of github actions. And in the case of `fsl_libs`, only one of those sub workflows was useful, publishing a crate to shipyard

As we are already using fslabscli to auto-generate our workflows and version checking, we&rsquo;ve dropped the use of the meta workflow, only bringing the required sub-workflow in the release pipeline.

We can see in how uncluttered the released graph now look for both our main repos
fslabscli-v1.x             |  fslabscli-v2.0.0
:-------------------------:|:-------------------------:
![](https://github.com/user-attachments/assets/eba3b83b-96a4-437d-ad7a-a566ff3554c4)  |  ![](https://github.com/user-attachments/assets/01b02f80-312f-4b9d-9245-186f2fafc6ab)
*orica-libs-publishing-v1.x* | *orica-libs-publishing-v2.0.0*
![](https://github.com/user-attachments/assets/93ac575b-7c41-44a7-86b7-b01c04f6a252)  | ![](https://github.com/user-attachments/assets/fb9c8fc0-cb61-4c38-9507-d5d957975355)
*fsl_libs-publishing-v1.x* | *fsl_libs-publishing-v2.0.0*

## Allow specifying precise workflow version
Ci workflows release are now back to being proper git tags and can be reference as such, breaking ci changes are not forced on the repo and the updaete can happen on a repo per repo basis.

## Workflow Runtime information is done in fslabscli
A lot of runtime information such as package name, package version, toolchain, ... are already knowed and extracted as part of fslabscli `check-workspace` command, using them as workflow inputs solve two issues:
  *  Runtime information remains the same throughout the workflow run: We used to need to compute the current date as part of each package run. This would/could be used to generate some filename and/or version. Let's say you have `app_a` depending on  `app_b` and both getting build in the same run, around midnight. We saw situation where the filename changed because the date changed and thus the workflow failing.
  * Ci cost: If you need a job to derive those information, even if this is lightweight compute, Github is billing is rounded the usage the nearest minutes [[1]](https://docs.github.com/en/billing/managing-billing-for-github-actions/about-billing-for-github-actions#included-storage-and-minutes). All those very small jobs were adding up. This also contributes to uncluttering the workflows and reduce the propability of hitting github undocumented workflow limit.
  
## Installer publishing need are now separate from their binary
We use to only check if a binary needed releasing, and use this information to trigger the installer release. Most often than not it was the case when the binary release worked fine, but not the installer, as the pipeline is most finicky. This meant that reruning the workflow would not work as it would be detected as released. The checking has now been split.
## Nightly version 
As part of <#workflow-runtime-information-is-done-in-fslabscli> the `nightly` version is computed in fslabscli. The logic is as follows:
  1. Compute timestamp since our custom EPOCH (2024-01-01) in days
  2. Append it to the current package version in `Cargo.toml`
  
  This means that a nightly version might look like this : `1.3.44.203`



# Missing from this release
## Release notification and tagging
The release pipelines no longer creates a discord notification nor a github release, this is High priority to bring back in 2.1.0
