# A kwok cluster (kube-apiserver, etcd and fake nodes as Windows programs: the runner has no
# Linux containers for kind) with a deployment, and a kubeconfig of two contexts on it: its
# admin certificate (kwok-recouvrance), and a login command (exec credential plugin, a console
# program) that prints the same certificate (kwok-exec, the current context).
$ErrorActionPreference = 'Stop'
$kwokctl = "$((New-Item -ItemType Directory -Force "$env:E2E\bin").FullName)\kwokctl.exe"
Invoke-WebRequest "https://github.com/kubernetes-sigs/kwok/releases/download/$env:KWOK_VERSION/kwokctl-windows-amd64.exe" -OutFile $kwokctl
$sum = (Get-FileHash $kwokctl -Algorithm SHA256).Hash
if ($sum -ne $env:KWOKCTL_SHA256) { throw "kwokctl: sha256 $sum" }
"KWOKCTL=$kwokctl" >> $env:GITHUB_ENV
& $kwokctl create cluster --name recouvrance --runtime binary --kubeconfig "$env:E2E\kwok.kubeconfig" --wait 5m
if ($LASTEXITCODE) { exit 1 }
& $kwokctl --name recouvrance kubectl apply -f "$PSScriptRoot\objects.yaml"
& $kwokctl --name recouvrance kubectl rollout status deployment/web --timeout=180s
if ($LASTEXITCODE) { exit 1 }
& $kwokctl --name recouvrance kubectl get nodes,pods -A -o wide

$cluster = "$env:USERPROFILE\.kwok\clusters\recouvrance"
$pki = "$cluster\pki"
$server = (Select-String -Path "$cluster\kubeconfig.yaml" -Pattern 'server: (\S+)').Matches[0].Groups[1].Value
"API server: $server"
@{
  apiVersion = 'client.authentication.k8s.io/v1'; kind = 'ExecCredential'
  status = @{ clientCertificateData = (Get-Content -Raw "$pki\admin.crt"); clientKeyData = (Get-Content -Raw "$pki\admin.key") }
} | ConvertTo-Json | Set-Content "$env:E2E\credential.json"
Copy-Item "$PSScriptRoot\login.ps1" $env:E2E
$login = @{
  apiVersion = 'client.authentication.k8s.io/v1'; interactiveMode = 'Never'; command = 'powershell.exe'
  args = @('-NoProfile', '-NonInteractive', '-ExecutionPolicy', 'Bypass', '-File', "$env:E2E\login.ps1")
}
[ordered]@{
  apiVersion = 'v1'; kind = 'Config'; 'current-context' = 'kwok-exec'
  clusters = @(@{ name = 'kwok'; cluster = @{ server = $server; 'certificate-authority' = "$pki\ca.crt" } })
  users = @(
    @{ name = 'admin'; user = @{ 'client-certificate' = "$pki\admin.crt"; 'client-key' = "$pki\admin.key" } },
    @{ name = 'login'; user = @{ exec = $login } }
  )
  contexts = @(
    @{ name = 'kwok-recouvrance'; context = @{ cluster = 'kwok'; user = 'admin'; namespace = 'default' } },
    @{ name = 'kwok-exec'; context = @{ cluster = 'kwok'; user = 'login'; namespace = 'default' } }
  )
} | ConvertTo-Json -Depth 10 | Set-Content $env:KUBECONFIG

# The login command works on its own.
kubectl --kubeconfig $env:KUBECONFIG --context kwok-exec get pods
if ($LASTEXITCODE) { exit 1 }
