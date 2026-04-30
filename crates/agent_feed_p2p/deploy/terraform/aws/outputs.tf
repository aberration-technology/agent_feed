output "edge_url" {
  description = "Public feed edge URL."
  value       = local.edge_url
}

output "browser_app_url" {
  description = "Public browser shell URL."
  value       = local.browser_app_base_url
}

output "browser_app_hostname" {
  description = "Public browser shell hostname."
  value       = local.browser_app_hostname_normalized
}

output "edge_instance_id" {
  description = "EC2 instance id for the feed edge."
  value       = aws_instance.edge.id
}

output "edge_public_ip" {
  description = "Elastic IP attached to the feed edge."
  value       = aws_eip.edge.public_ip
}

output "seed_node_tcp_multiaddr" {
  description = "TCP bootstrap multiaddr advertised to native peers."
  value       = local.seed_node_tcp_multiaddr
}

output "seed_node_quic_multiaddr" {
  description = "QUIC bootstrap multiaddr advertised to native peers."
  value       = local.seed_node_quic_multiaddr
}

output "seed_node_webrtc_direct_multiaddr" {
  description = "WebRTC Direct bootstrap multiaddr advertised to browser peers."
  value       = local.seed_node_webrtc_direct_multiaddr
}

output "route_canary_url" {
  description = "Deep-link canary URL for the browser shell."
  value       = "${local.browser_app_base_url}/${var.canary_github_login}?all"
}

output "edge_resolver_url" {
  description = "Canary GitHub resolver URL."
  value       = "${local.edge_url}/resolve/github/${var.canary_github_login}"
}

output "rendered_caddyfile" {
  description = "Rendered Caddyfile synced to the edge host by the deploy workflow."
  value       = local.caddyfile
}

output "rendered_edge_env" {
  description = "Rendered non-secret edge environment synced to the edge host by the deploy workflow."
  value       = local.edge_env
}

output "rendered_edge_toml" {
  description = "Rendered edge TOML config synced to the edge host by the deploy workflow."
  value       = local.edge_toml
}

output "rendered_edge_service_unit" {
  description = "Rendered systemd unit synced to the edge host by the deploy workflow."
  value       = local.edge_service_unit
}

output "secret_parameter_prefix" {
  description = "SSM parameter prefix read by the edge host."
  value       = var.secret_parameter_prefix
}
