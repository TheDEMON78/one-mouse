# Micro par défaut de Windows (one-mouse).
#   $env:ONE_MOUSE_MIC_TARGET  : fragment du nom du micro à mettre par défaut (ex. « CABLE Output »)
#   $env:ONE_MOUSE_MIC_RESTORE : identifiant d'un micro à remettre par défaut
# Affiche l'identifiant du micro par défaut avant le changement.
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

public static class OneMouseMic {
    public static string GetDefault() {
        var enumerator = (IMMDeviceEnumerator)new MMDeviceEnumerator();
        IMMDevice device;
        // 1 = capture, 0 = rôle « console ».
        if (enumerator.GetDefaultAudioEndpoint(1, 0, out device) != 0) return "";
        string id; device.GetId(out id); return id;
    }
    public static void SetDefault(string id) {
        var policy = (IPolicyConfig)new PolicyConfigClient();
        // Rôles console, multimédia et communications.
        for (int role = 0; role < 3; role++) Marshal.ThrowExceptionForHR(policy.SetDefaultEndpoint(id, role));
    }
}
"@

$previous = [OneMouseMic]::GetDefault()
Write-Output $previous

if ($env:ONE_MOUSE_MIC_RESTORE) {
    [OneMouseMic]::SetDefault($env:ONE_MOUSE_MIC_RESTORE)
    exit 0
}

$root = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\MMDevices\Audio\Capture'
$nameKey = '{a45c254e-df1c-4efd-8020-67d146a850e0},2'
foreach ($endpoint in Get-ChildItem $root) {
    $state = (Get-ItemProperty $endpoint.PSPath).DeviceState
    $name = (Get-ItemProperty "$($endpoint.PSPath)\Properties" -ErrorAction SilentlyContinue).$nameKey
    if ($state -eq 1 -and $name -like "*$($env:ONE_MOUSE_MIC_TARGET)*") {
        [OneMouseMic]::SetDefault('{0.0.1.00000000}.' + $endpoint.PSChildName)
        exit 0
    }
}
exit 2
