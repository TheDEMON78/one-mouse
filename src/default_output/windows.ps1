# Sortie son par défaut de Windows (one-mouse).
# Si la sortie par défaut est un câble audio virtuel (VB-Cable met souvent
# « CABLE Input » par défaut à son installation), remet par défaut une vraie
# sortie : sinon le micro reçu de l'autre PC, joué dans ce câble, serait capturé
# avec le son du PC et renvoyé (on s'entend en écho).
# Affiche le nom de la sortie remise par défaut. Codes : 0 changée, 3 rien à
# faire, 2 aucune vraie sortie.
$ErrorActionPreference = 'Stop'
Add-Type -TypeDefinition @"
using System;
using System.Runtime.InteropServices;

[Guid("f8679f50-850a-41cf-9c72-430f290290c8"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IPolicyConfig {
    int GetMixFormat(); int GetDeviceFormat(); int ResetDeviceFormat(); int SetDeviceFormat();
    int GetProcessingPeriod(); int SetProcessingPeriod(); int GetShareMode(); int SetShareMode();
    int GetPropertyValue(); int SetPropertyValue();
    [PreserveSig] int SetDefaultEndpoint([MarshalAs(UnmanagedType.LPWStr)] string id, int role);
    int SetEndpointVisibility();
}
[ComImport, Guid("870af99c-171d-4f9e-af0d-e63df40c2bc9")] class PolicyConfigClient {}

[Guid("D666063F-1587-4E43-81F1-B948E807363F"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDevice {
    int Activate(); int OpenPropertyStore();
    [PreserveSig] int GetId([MarshalAs(UnmanagedType.LPWStr)] out string id);
}
[Guid("A95664D2-9614-4F35-A746-DE8DB63617E6"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
interface IMMDeviceEnumerator {
    int EnumAudioEndpoints();
    [PreserveSig] int GetDefaultAudioEndpoint(int dataFlow, int role, out IMMDevice device);
}
[ComImport, Guid("BCDE0395-E52F-467C-8E3D-C4579291692E")] class MMDeviceEnumerator {}

public static class OneMouseOutput {
    // 0 = sortie (render) ; rôle 0 = console, 2 = communications.
    public static string GetDefault(int role) {
        var enumerator = (IMMDeviceEnumerator)new MMDeviceEnumerator();
        IMMDevice device;
        if (enumerator.GetDefaultAudioEndpoint(0, role, out device) != 0) return "";
        string id; device.GetId(out id); return id;
    }
    public static void SetDefault(string id) {
        var policy = (IPolicyConfig)new PolicyConfigClient();
        for (int role = 0; role < 3; role++) Marshal.ThrowExceptionForHR(policy.SetDefaultEndpoint(id, role));
    }
}
"@

$root = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Render'
$prefix = '{0.0.0.00000000}.'

function Get-Name($guid) {
    $props = Get-ItemProperty "$root\$guid\Properties" -ErrorAction SilentlyContinue
    if (-not $props) { return '' }
    $desc = $props.'{a45c254e-df1c-4efd-8020-67d146a850e0},2'
    $iface = $props.'{b3f8fa53-0004-438e-9003-51a46e139bfc},6'
    return "$desc ($iface)"
}

function Test-Virtual($name) {
    return $name -match 'CABLE|VB-Audio|BlackHole|one-mouse|one_mouse|Virtual Audio'
}

function Get-Guid($id) {
    if ($id.StartsWith($prefix)) { return $id.Substring($prefix.Length) }
    return ''
}

$current = Get-Guid ([OneMouseOutput]::GetDefault(0))
if (-not $current -or -not (Test-Virtual (Get-Name $current))) { exit 3 }

# De préférence la sortie « communications » si elle est réelle, sinon la
# première sortie active qui n'est pas un câble virtuel.
$choice = Get-Guid ([OneMouseOutput]::GetDefault(2))
if (-not $choice -or (Test-Virtual (Get-Name $choice))) {
    $choice = ''
    foreach ($endpoint in Get-ChildItem $root) {
        $state = (Get-ItemProperty $endpoint.PSPath).DeviceState
        if ($state -eq 1 -and -not (Test-Virtual (Get-Name $endpoint.PSChildName))) {
            $choice = $endpoint.PSChildName
            break
        }
    }
}
if (-not $choice) { exit 2 }
[OneMouseOutput]::SetDefault($prefix + $choice)
Write-Output (Get-Name $choice)
exit 0
