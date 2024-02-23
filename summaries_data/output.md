<details>
<summary><h2>
ci_bis_tests - ✅
</h2></summary>
<pre lang=mermaid><code>flowchart LR
    subgraph Check
        direction LR
            fmt-->check-->clippy-->doc-->deny_license-->deny_bans-->deny_advisories-->deny_sources-->dependencies-->publish_dryrun
            click fmt "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:15:1" "logs"
            style fmt fill:green
            click check "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:16:1" "logs"
            style check fill:green
            click clippy "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:17:1" "logs"
            style clippy fill:green
            click doc "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:18:1" "logs"
            style doc fill:green
            click deny_license "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:20:1" "logs"
            style deny_license fill:green
            click deny_bans "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:21:1" "logs"
            style deny_bans fill:green
            click deny_advisories "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:22:1" "logs"
            style deny_advisories fill:green
            click deny_sources "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:23:1" "logs"
            style deny_sources fill:green
            click dependencies "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:24:1" "logs"
            style dependencies fill:green
            click publish_dryrun "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:25:1" "logs"
            style publish_dryrun fill:green
        end
    style Check stroke:green

    subgraph Test
        direction LR
            tests
            click tests "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450469#step:26:1" "logs"
            style tests fill:green
        end
    style Test stroke:green

    subgraph Miri
        direction LR
            miri
            click miri "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450743#step:27:1" "logs"
            style miri fill:red
        end
    style Miri stroke:green
</code></pre>
</details>
<details>
<summary><h2>
ci_tests - ✅
</h2></summary>
<pre lang=mermaid><code>flowchart LR
    subgraph Check
        direction LR
            fmt-->check-->clippy-->doc-->deny_license-->deny_bans-->deny_advisories-->deny_sources-->dependencies-->publish_dryrun
            click fmt "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:15:1" "logs"
            style fmt fill:green
            click check "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:16:1" "logs"
            style check fill:green
            click clippy "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:17:1" "logs"
            style clippy fill:green
            click doc "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:18:1" "logs"
            style doc fill:green
            click deny_license "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:20:1" "logs"
            style deny_license fill:green
            click deny_bans "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:21:1" "logs"
            style deny_bans fill:green
            click deny_advisories "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:22:1" "logs"
            style deny_advisories fill:green
            click deny_sources "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:23:1" "logs"
            style deny_sources fill:green
            click dependencies "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:24:1" "logs"
            style dependencies fill:green
            click publish_dryrun "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450175#step:25:1" "logs"
            style publish_dryrun fill:green
        end
    style Check stroke:green

    subgraph Miri
        direction LR
            miri
            click miri "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450743#step:27:1" "logs"
            style miri fill:red
        end
    style Miri stroke:green

    subgraph Test
        direction LR
            tests
            click tests "https://github.com/ForesightMiningSoftwareCorporation/ci_tests/actions/runs/8017778855/job/21919450469#step:26:1" "logs"
            style tests fill:green
        end
    style Test stroke:green
</code></pre>
</details>
