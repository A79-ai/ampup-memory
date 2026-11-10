output "bucket" { value = aws_s3_bucket.data.bucket }
output "engine_role_arn" { value = aws_iam_role.engine.arn }
output "agent_role_arn" { value = aws_iam_role.agent.arn }
output "cluster_role_arn" { value = aws_iam_role.cluster.arn }
output "nodes_role_arn" { value = aws_iam_role.nodes.arn }
output "engine_policy" { value = data.aws_iam_policy_document.engine.json }
output "trust_policies" { value = { for k, d in data.aws_iam_policy_document.trust : k => d.json } }
