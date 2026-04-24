# feed p2p deploy

production uses the same split-host shape as the burn_dragon p2p deployment:

```text
feed.aberration.technology
  static github pages browser shell and username deep links

edge.feed.aberration.technology
  github resolver, browser seeds, directory, rendezvous/bootstrap edge
```

the local product stays local. the hosted product is called `feed` in the UI.
`agent_reel` remains the crate and binary family.

## github actions

operator entrypoints:

```text
.github/workflows/deploy-agent-reel-p2p-aws.yml
.github/workflows/deploy-pages.yml
.github/workflows/inspect-agent-reel-p2p-aws.yml
.github/workflows/cleanup-agent-reel-p2p-aws.yml
```

the aws deploy workflow owns terraform plan/apply for the edge and Route53
records. after a successful apply it can dispatch the pages publish workflow so
the browser shell is ordered behind the live edge.

the pages workflow builds a static shell with:

```text
cargo run -p xtask -- build-browser-site
```

it writes:

```text
index.html
404.html
feed-config.json
CNAME
```

`404.html` is intentional. it keeps deep links such as `/mosure?all` working on
github pages.

## github environment

use the existing environment:

```text
agent-real-p2p-production
```

required variable:

```text
AGENT_FEED_P2P_AWS_ROLE_ARN
```

recommended variables:

```text
AGENT_FEED_P2P_AWS_REGION=us-east-2
AGENT_FEED_P2P_STACK_NAME=agent-feed-p2p-production
AGENT_FEED_P2P_EDGE_DOMAIN_NAME=edge.feed.aberration.technology
AGENT_FEED_P2P_EDGE_BASE_URL=https://edge.feed.aberration.technology
AGENT_FEED_P2P_BROWSER_APP_BASE_URL=https://feed.aberration.technology
AGENT_FEED_P2P_BROWSER_APP_PAGES_DOMAIN_TARGET=aberration-technology.github.io
AGENT_FEED_P2P_ROUTE53_ZONE_NAME=aberration.technology
AGENT_FEED_P2P_NETWORK_ID=agent-reel-mainnet
AGENT_FEED_P2P_CANARY_GITHUB_LOGIN=mosure
AGENT_FEED_P2P_CANARY_FEED_LABEL=workstation
```

optional variables:

```text
AGENT_FEED_P2P_AWS_CLEANUP_ROLE_ARN
AGENT_FEED_P2P_GITHUB_REQUIRED_ORG
AGENT_FEED_P2P_GITHUB_REQUIRED_REPO
AGENT_FEED_P2P_GITHUB_ADMIN_LOGINS
AGENT_FEED_P2P_ALARM_SNS_TOPIC_ARN
```

existing secrets:

```text
AGENT_FEED_P2P_GITHUB_CLIENT_ID
AGENT_FEED_P2P_GITHUB_CLIENT_SECRET
AGENT_FEED_P2P_OAUTH_CLIENT_ID
AGENT_FEED_P2P_OAUTH_CLIENT_SECRET
```

terraform reads OAuth material from SSM on the host path. workflows do not print
secret values.

## terraform

root:

```text
crates/agent_reel_p2p/deploy/terraform/aws
```

the stack manages:

```text
one ec2 edge host
one elastic ip
one small public vpc/subnet
route53 A record for edge.feed.aberration.technology
route53 CNAME for feed.aberration.technology -> github pages
caddy tls termination on the host
ssm-enabled instance role
basic cloudwatch status alarm
```

guardrail:

```text
allow_route53_zone_apex_records = false
```

terraform refuses to manage `aberration.technology` itself unless that guardrail
is explicitly disabled.

## canary

deploy is not considered green until these pass:

```text
https://feed.aberration.technology/{canary_github_login}?all loads the static shell
https://edge.feed.aberration.technology/healthz returns ok
```

resolver and feed-discovery checks can be tightened as live publisher directory
state becomes available.
