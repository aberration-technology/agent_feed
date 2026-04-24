variable "aws_region" {
  description = "AWS region for the feed p2p edge."
  type        = string
  default     = "us-east-2"
}

variable "environment_name" {
  description = "Deployment environment label."
  type        = string
  default     = "production"
}

variable "stack_name" {
  description = "Logical stack name used for tags and resources."
  type        = string
  default     = "agent-feed-p2p-production"

  validation {
    condition     = can(regex("^agent-feed-p2p(?:-[a-z0-9][a-z0-9-]*)?$", var.stack_name))
    error_message = "stack_name must use the canonical agent-feed-p2p-* naming scheme."
  }
}

variable "route53_zone_name" {
  description = "Public Route53 hosted zone containing feed subdomains."
  type        = string
  default     = "aberration.technology"
}

variable "browser_app_base_url" {
  description = "Public GitHub Pages browser shell base URL."
  type        = string
  default     = "https://feed.aberration.technology"
}

variable "edge_domain_name" {
  description = "Public HTTPS hostname for the feed edge."
  type        = string
  default     = "edge.feed.aberration.technology"
}

variable "browser_app_pages_domain_target" {
  description = "GitHub Pages origin host used by the edge to fetch the browser shell."
  type        = string
  default     = "aberration-technology.github.io"
}

variable "browser_app_pages_base_path" {
  description = "Path prefix for the GitHub Pages browser shell origin."
  type        = string
  default     = "/agent_feed"
}

variable "github_callback_url" {
  description = "GitHub OAuth authorization callback URL registered on the OAuth app."
  type        = string
  default     = ""
}

variable "allow_route53_zone_apex_records" {
  description = "Whether Terraform may manage records at the hosted-zone apex."
  type        = bool
  default     = false
}

variable "network_id" {
  description = "Feed network id."
  type        = string
  default     = "agent-feed-mainnet"
}

variable "instance_type" {
  description = "EC2 instance type for the edge."
  type        = string
  default     = "t3a.nano"
}

variable "root_volume_size_gib" {
  description = "Root volume size for the edge host."
  type        = number
  default     = 24
}

variable "ssh_cidr_blocks" {
  description = "Optional SSH ingress CIDR blocks. Empty disables SSH ingress."
  type        = list(string)
  default     = []
}

variable "p2p_port" {
  description = "Native TCP/QUIC feed p2p port."
  type        = number
  default     = 7747
}

variable "edge_loopback_port" {
  description = "Loopback HTTP port for the edge daemon."
  type        = number
  default     = 7778
}

variable "agent_feed_install_source" {
  description = "How the edge host installs agent-feed. Supported values: s3, git, or crate."
  type        = string
  default     = "s3"

  validation {
    condition     = contains(["s3", "git", "crate"], lower(trimspace(var.agent_feed_install_source)))
    error_message = "agent_feed_install_source must be s3, git, or crate."
  }
}

variable "agent_feed_binary_s3_uri" {
  description = "S3 URI for a prebuilt agent-feed binary when agent_feed_install_source = s3."
  type        = string
  default     = ""
}

variable "agent_feed_binary_s3_bucket" {
  description = "S3 bucket containing the prebuilt agent-feed binary."
  type        = string
  default     = ""
}

variable "agent_feed_binary_s3_key" {
  description = "S3 object key containing the prebuilt agent-feed binary."
  type        = string
  default     = ""
}

variable "agent_feed_git_repository" {
  description = "Git repository used when agent_feed_install_source = git."
  type        = string
  default     = "https://github.com/aberration-technology/agent_feed.git"
}

variable "agent_feed_git_ref" {
  description = "Git ref used when agent_feed_install_source = git."
  type        = string
  default     = "main"
}

variable "agent_feed_crate_version" {
  description = "Crate version used when agent_feed_install_source = crate."
  type        = string
  default     = "0.1.0"
}

variable "github_required_org" {
  description = "Optional GitHub org required by the edge auth policy."
  type        = string
  default     = ""
}

variable "github_required_teams" {
  description = "Optional comma-separated GitHub team slugs required inside github_required_org."
  type        = string
  default     = ""
}

variable "github_required_repo" {
  description = "Optional GitHub repo required by the edge auth policy."
  type        = string
  default     = ""
}

variable "github_admin_logins" {
  description = "GitHub logins with edge admin rights."
  type        = list(string)
  default     = []
}

variable "secret_parameter_prefix" {
  description = "SSM parameter prefix for OAuth and edge authority material."
  type        = string
  default     = "/agent-feed-p2p/mainnet/edge"
}

variable "github_client_id_parameter_name" {
  description = "SSM SecureString parameter name containing the GitHub OAuth client id."
  type        = string
  default     = "/agent-feed-p2p/mainnet/edge/github_client_id"
}

variable "github_client_secret_parameter_name" {
  description = "SSM SecureString parameter name containing the GitHub OAuth client secret."
  type        = string
  default     = "/agent-feed-p2p/mainnet/edge/github_client_secret"
}

variable "canary_github_login" {
  description = "GitHub login used by the live route canary."
  type        = string
  default     = "mosure"
}

variable "canary_feed_label" {
  description = "Feed label used by the live route canary."
  type        = string
  default     = "workstation"
}

variable "alarm_sns_topic_arn" {
  description = "Optional SNS topic receiving edge health alarms."
  type        = string
  default     = ""
}

variable "enable_cloudwatch_alarms" {
  description = "Whether to create basic EC2 status alarms."
  type        = bool
  default     = true
}

variable "tags" {
  description = "Additional tags for managed resources."
  type        = map(string)
  default     = {}
}
