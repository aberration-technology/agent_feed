data "aws_caller_identity" "current" {}

data "aws_partition" "current" {}

data "aws_route53_zone" "selected" {
  name         = endswith(var.route53_zone_name, ".") ? var.route53_zone_name : "${var.route53_zone_name}."
  private_zone = false
}

data "aws_availability_zones" "available" {
  state = "available"
}

data "aws_ami" "ubuntu" {
  most_recent = true
  owners      = ["099720109477"]

  filter {
    name   = "name"
    values = ["ubuntu/images/hvm-ssd-gp3/ubuntu-noble-24.04-amd64-server-*"]
  }

  filter {
    name   = "virtualization-type"
    values = ["hvm"]
  }

  filter {
    name   = "architecture"
    values = ["x86_64"]
  }
}

data "aws_cloudfront_cache_policy" "caching_disabled" {
  name = "Managed-CachingDisabled"
}

data "aws_cloudfront_origin_request_policy" "all_viewer_except_host_header" {
  name = "Managed-AllViewerExceptHostHeader"
}

locals {
  route53_zone_apex           = trimsuffix(lower(trimspace(var.route53_zone_name)), ".")
  edge_domain_name_normalized = trimsuffix(lower(trimspace(var.edge_domain_name)), ".")
  bootstrap_domain_name_normalized = trimsuffix(
    lower(trimspace(var.bootstrap_domain_name)),
    ".",
  )
  browser_app_base_url = trimsuffix(trimspace(var.browser_app_base_url), "/")
  browser_app_hostname = split(
    "/",
    replace(replace(local.browser_app_base_url, "https://", ""), "http://", ""),
  )[0]
  browser_app_hostname_normalized   = trimsuffix(lower(local.browser_app_hostname), ".")
  browser_app_pages_domain_target   = trimsuffix(trimspace(var.browser_app_pages_domain_target), ".")
  github_callback_url               = trimspace(var.github_callback_url) == "" ? "${local.edge_url}/callback/github" : trimspace(var.github_callback_url)
  claiming_edge_apex                = local.edge_domain_name_normalized == local.route53_zone_apex
  claiming_bootstrap_apex           = local.bootstrap_domain_name_normalized == local.route53_zone_apex
  claiming_browser_apex             = local.browser_app_hostname_normalized == local.route53_zone_apex
  cloudwatch_alarm_actions          = trimspace(var.alarm_sns_topic_arn) == "" ? [] : [trimspace(var.alarm_sns_topic_arn)]
  agent_feed_install_source         = lower(trimspace(var.agent_feed_install_source))
  github_admin_logins_csv           = join(",", var.github_admin_logins)
  edge_url                          = "https://${local.edge_domain_name_normalized}"
  seed_node_tcp_multiaddr           = "/dns4/${local.bootstrap_domain_name_normalized}/tcp/${var.p2p_port}"
  seed_node_quic_multiaddr          = "/dns4/${local.bootstrap_domain_name_normalized}/udp/${var.p2p_port}/quic-v1"
  seed_node_webrtc_direct_multiaddr = "/dns4/${local.bootstrap_domain_name_normalized}/udp/443/webrtc-direct"
  tags = merge(
    var.tags,
    {
      Application        = "agent-feed-p2p"
      Environment        = var.environment_name
      ManagedBy          = "terraform"
      Stack              = var.stack_name
      TerraformWorkspace = terraform.workspace
    },
  )
  binary_s3_object_arn = (
    trimspace(var.agent_feed_binary_s3_bucket) == "" || trimspace(var.agent_feed_binary_s3_key) == ""
    ? ""
    : "arn:${data.aws_partition.current.partition}:s3:::${var.agent_feed_binary_s3_bucket}/${var.agent_feed_binary_s3_key}"
  )

  edge_env = templatefile("${path.module}/templates/edge.env.tftpl", {
    browser_app_base_url                = local.browser_app_base_url
    edge_base_url                       = local.edge_url
    bootstrap_domain_name               = local.bootstrap_domain_name_normalized
    github_callback_url                 = local.github_callback_url
    network_id                          = var.network_id
    p2p_port                            = var.p2p_port
    github_required_org                 = var.github_required_org
    github_required_teams               = var.github_required_teams
    github_required_repo                = var.github_required_repo
    github_admin_logins                 = local.github_admin_logins_csv
    github_client_id_parameter_name     = var.github_client_id_parameter_name
    github_client_secret_parameter_name = var.github_client_secret_parameter_name
    canary_github_login                 = var.canary_github_login
    canary_feed_label                   = var.canary_feed_label
  })
  edge_toml = templatefile("${path.module}/templates/edge.toml.tftpl", {
    browser_app_base_url  = local.browser_app_base_url
    edge_base_url         = local.edge_url
    bootstrap_domain_name = local.bootstrap_domain_name_normalized
    github_callback_url   = local.github_callback_url
    network_id            = var.network_id
    p2p_port              = var.p2p_port
    edge_loopback_port    = var.edge_loopback_port
    github_required_org   = var.github_required_org
    github_required_teams = var.github_required_teams
  })
  caddyfile = templatefile("${path.module}/templates/Caddyfile.tftpl", {
    browser_app_hostname            = local.browser_app_hostname
    browser_app_origin              = local.browser_app_base_url
    browser_app_pages_domain_target = local.browser_app_pages_domain_target
    bootstrap_domain_name           = local.bootstrap_domain_name_normalized
    edge_domain_name                = local.edge_domain_name_normalized
    edge_loopback_port              = var.edge_loopback_port
    tls_contact_email               = var.tls_contact_email
  })
  edge_service_unit = templatefile("${path.module}/templates/agent-feed-edge.service.tftpl", {
    bootstrap_domain_name = local.bootstrap_domain_name_normalized
    browser_app_base_url  = local.browser_app_base_url
    edge_base_url         = local.edge_url
    edge_loopback_port    = var.edge_loopback_port
    github_callback_url   = local.github_callback_url
    network_id            = var.network_id
  })
  user_data = templatefile("${path.module}/templates/user-data.sh.tftpl", {
    agent_feed_binary_s3_uri   = var.agent_feed_binary_s3_uri
    agent_feed_crate_version   = var.agent_feed_crate_version
    agent_feed_git_ref         = var.agent_feed_git_ref
    agent_feed_git_repository  = var.agent_feed_git_repository
    agent_feed_install_source  = local.agent_feed_install_source
    caddyfile                  = local.caddyfile
    edge_env                   = local.edge_env
    edge_service_unit          = local.edge_service_unit
    edge_toml                  = local.edge_toml
    github_client_id_parameter = var.github_client_id_parameter_name
    github_client_secret_param = var.github_client_secret_parameter_name
    secret_parameter_prefix    = var.secret_parameter_prefix
  })
}

resource "terraform_data" "apex_guardrail" {
  input = {
    bootstrap = local.claiming_bootstrap_apex
    browser   = local.claiming_browser_apex
    edge      = local.claiming_edge_apex
  }

  lifecycle {
    precondition {
      condition = (
        (!local.claiming_browser_apex && !local.claiming_edge_apex && !local.claiming_bootstrap_apex)
        || var.allow_route53_zone_apex_records
      )
      error_message = "refusing to manage hosted-zone apex records without explicit override"
    }
  }
}

resource "aws_vpc" "edge" {
  cidr_block           = "10.77.0.0/16"
  enable_dns_hostnames = true
  enable_dns_support   = true

  tags = merge(local.tags, {
    Name = "${var.stack_name}-vpc"
  })
}

resource "aws_internet_gateway" "edge" {
  vpc_id = aws_vpc.edge.id

  tags = merge(local.tags, {
    Name = "${var.stack_name}-igw"
  })
}

resource "aws_subnet" "edge" {
  vpc_id                  = aws_vpc.edge.id
  cidr_block              = "10.77.1.0/24"
  availability_zone       = data.aws_availability_zones.available.names[0]
  map_public_ip_on_launch = true

  tags = merge(local.tags, {
    Name = "${var.stack_name}-public-a"
  })
}

resource "aws_route_table" "edge" {
  vpc_id = aws_vpc.edge.id

  route {
    cidr_block = "0.0.0.0/0"
    gateway_id = aws_internet_gateway.edge.id
  }

  tags = merge(local.tags, {
    Name = "${var.stack_name}-public"
  })
}

resource "aws_route_table_association" "edge" {
  subnet_id      = aws_subnet.edge.id
  route_table_id = aws_route_table.edge.id
}

resource "aws_security_group" "edge" {
  name        = "${var.stack_name}-edge"
  description = "feed edge ingress"
  vpc_id      = aws_vpc.edge.id

  ingress {
    description = "http redirect"
    from_port   = 80
    to_port     = 80
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  ingress {
    description = "https edge"
    from_port   = 443
    to_port     = 443
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  ingress {
    description = "webrtc-direct"
    from_port   = 443
    to_port     = 443
    protocol    = "udp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  ingress {
    description = "native p2p tcp"
    from_port   = var.p2p_port
    to_port     = var.p2p_port
    protocol    = "tcp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  ingress {
    description = "native p2p quic"
    from_port   = var.p2p_port
    to_port     = var.p2p_port
    protocol    = "udp"
    cidr_blocks = ["0.0.0.0/0"]
  }

  dynamic "ingress" {
    for_each = length(var.ssh_cidr_blocks) == 0 ? [] : [1]

    content {
      description = "operator ssh"
      from_port   = 22
      to_port     = 22
      protocol    = "tcp"
      cidr_blocks = var.ssh_cidr_blocks
    }
  }

  egress {
    description = "all outbound"
    from_port   = 0
    to_port     = 0
    protocol    = "-1"
    cidr_blocks = ["0.0.0.0/0"]
  }

  tags = merge(local.tags, {
    Name = "${var.stack_name}-edge"
  })
}

resource "aws_iam_role" "edge" {
  name = "${var.stack_name}-edge"

  assume_role_policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Action = "sts:AssumeRole"
        Effect = "Allow"
        Principal = {
          Service = "ec2.amazonaws.com"
        }
      },
    ]
  })

  tags = local.tags
}

resource "aws_iam_role_policy_attachment" "ssm_managed_instance" {
  role       = aws_iam_role.edge.name
  policy_arn = "arn:${data.aws_partition.current.partition}:iam::aws:policy/AmazonSSMManagedInstanceCore"
}

resource "aws_iam_role_policy" "edge_ssm" {
  name = "${var.stack_name}-edge-ssm"
  role = aws_iam_role.edge.id

  policy = jsonencode({
    Version = "2012-10-17"
    Statement = [
      {
        Effect = "Allow"
        Action = [
          "ssm:GetParameter",
          "ssm:GetParameters",
          "ssm:PutParameter",
        ]
        Resource = [
          "arn:${data.aws_partition.current.partition}:ssm:${var.aws_region}:${data.aws_caller_identity.current.account_id}:parameter${var.secret_parameter_prefix}/*",
        ]
      },
      {
        Effect = "Allow"
        Action = [
          "kms:Decrypt",
          "kms:Encrypt",
        ]
        Resource = "*"
      },
      {
        Effect = "Allow"
        Action = [
          "s3:GetObject",
        ]
        Resource = compact([
          local.binary_s3_object_arn,
        ])
      },
    ]
  })
}

resource "aws_iam_instance_profile" "edge" {
  name = "${var.stack_name}-edge"
  role = aws_iam_role.edge.name

  tags = local.tags
}

resource "aws_instance" "edge" {
  ami                         = data.aws_ami.ubuntu.id
  instance_type               = var.instance_type
  subnet_id                   = aws_subnet.edge.id
  vpc_security_group_ids      = [aws_security_group.edge.id]
  iam_instance_profile        = aws_iam_instance_profile.edge.name
  associate_public_ip_address = true
  user_data_replace_on_change = false
  user_data                   = local.user_data

  metadata_options {
    http_endpoint               = "enabled"
    http_tokens                 = "required"
    http_put_response_hop_limit = 1
  }

  root_block_device {
    volume_size           = var.root_volume_size_gib
    volume_type           = "gp3"
    encrypted             = true
    delete_on_termination = true
  }

  tags = merge(local.tags, {
    Name = "${var.stack_name}-edge"
  })
}

resource "aws_eip" "edge" {
  domain = "vpc"

  tags = merge(local.tags, {
    Name = "${var.stack_name}-edge"
  })
}

resource "aws_eip_association" "edge" {
  allocation_id = aws_eip.edge.id
  instance_id   = aws_instance.edge.id
}

resource "aws_route53_record" "bootstrap" {
  zone_id         = data.aws_route53_zone.selected.zone_id
  name            = local.bootstrap_domain_name_normalized
  type            = "A"
  ttl             = 60
  records         = [aws_eip.edge.public_ip]
  allow_overwrite = true
}

resource "aws_acm_certificate" "edge" {
  provider          = aws.us_east_1
  domain_name       = local.edge_domain_name_normalized
  validation_method = "DNS"

  lifecycle {
    create_before_destroy = true
  }

  depends_on = [aws_route53_record.edge_caa]

  tags = merge(local.tags, {
    Name = "${var.stack_name}-edge-api"
  })
}

resource "aws_route53_record" "edge_cert_validation" {
  for_each = {
    for option in aws_acm_certificate.edge.domain_validation_options : option.domain_name => {
      name   = option.resource_record_name
      record = option.resource_record_value
      type   = option.resource_record_type
    }
  }

  zone_id         = data.aws_route53_zone.selected.zone_id
  name            = each.value.name
  type            = each.value.type
  ttl             = 60
  records         = [each.value.record]
  allow_overwrite = true
}

resource "aws_acm_certificate_validation" "edge" {
  provider                = aws.us_east_1
  certificate_arn         = aws_acm_certificate.edge.arn
  validation_record_fqdns = [for record in aws_route53_record.edge_cert_validation : record.fqdn]
}

resource "aws_cloudfront_distribution" "edge" {
  enabled         = true
  is_ipv6_enabled = true
  comment         = "${var.stack_name} feed api"
  aliases         = [local.edge_domain_name_normalized]
  price_class     = "PriceClass_100"
  http_version    = "http2"

  origin {
    domain_name = local.bootstrap_domain_name_normalized
    origin_id   = "feed-edge-http"

    custom_origin_config {
      http_port              = 80
      https_port             = 443
      origin_protocol_policy = "http-only"
      origin_ssl_protocols   = ["TLSv1.2"]
    }
  }

  default_cache_behavior {
    target_origin_id       = "feed-edge-http"
    viewer_protocol_policy = "redirect-to-https"
    allowed_methods        = ["GET", "HEAD", "OPTIONS", "PUT", "POST", "PATCH", "DELETE"]
    cached_methods         = ["GET", "HEAD", "OPTIONS"]
    compress               = true
    cache_policy_id        = data.aws_cloudfront_cache_policy.caching_disabled.id
    origin_request_policy_id = (
      data.aws_cloudfront_origin_request_policy.all_viewer_except_host_header.id
    )
  }

  restrictions {
    geo_restriction {
      restriction_type = "none"
    }
  }

  viewer_certificate {
    acm_certificate_arn      = aws_acm_certificate_validation.edge.certificate_arn
    minimum_protocol_version = "TLSv1.2_2021"
    ssl_support_method       = "sni-only"
  }

  tags = merge(local.tags, {
    Name = "${var.stack_name}-edge-api"
  })

  depends_on = [aws_route53_record.bootstrap]
}

resource "aws_route53_record" "edge" {
  zone_id = data.aws_route53_zone.selected.zone_id
  name    = local.edge_domain_name_normalized
  type    = "A"

  alias {
    name                   = aws_cloudfront_distribution.edge.domain_name
    zone_id                = aws_cloudfront_distribution.edge.hosted_zone_id
    evaluate_target_health = false
  }

  allow_overwrite = true
}

resource "aws_route53_record" "edge_caa" {
  zone_id = data.aws_route53_zone.selected.zone_id
  name    = local.edge_domain_name_normalized
  type    = "CAA"
  ttl     = 300
  records = [
    "0 issue \"amazon.com\"",
    "0 issue \"amazonaws.com\"",
    "0 issue \"amazontrust.com\"",
    "0 issue \"awstrust.com\"",
    "0 issue \"letsencrypt.org\"",
    "0 issue \"sectigo.com\"",
    "0 issue \"zerossl.com\"",
  ]
  allow_overwrite = true
}

resource "aws_route53_record" "browser" {
  zone_id         = data.aws_route53_zone.selected.zone_id
  name            = local.browser_app_hostname_normalized
  type            = "CNAME"
  ttl             = 300
  records         = [local.browser_app_pages_domain_target]
  allow_overwrite = true
}

resource "aws_cloudwatch_metric_alarm" "edge_status" {
  count = var.enable_cloudwatch_alarms ? 1 : 0

  alarm_name          = "${var.stack_name}-edge-status-check"
  comparison_operator = "GreaterThanThreshold"
  evaluation_periods  = 2
  metric_name         = "StatusCheckFailed"
  namespace           = "AWS/EC2"
  period              = 60
  statistic           = "Maximum"
  threshold           = 0
  alarm_description   = "Feed edge EC2 status check failed."
  alarm_actions       = local.cloudwatch_alarm_actions
  ok_actions          = local.cloudwatch_alarm_actions

  dimensions = {
    InstanceId = aws_instance.edge.id
  }

  tags = local.tags
}
