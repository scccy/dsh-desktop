using DshDesktop.WinUI.Native;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace DshDesktop.WinUI.Views;

/// <summary>
/// Read-only runtime facts. Everything shown here comes from the Rust core's
/// <c>dsh_diagnose</c>, so the settings screen cannot disagree with what the
/// launcher actually did.
/// </summary>
public sealed partial class SettingsView : UserControl
{
    public event Action? BackRequested;
    public event Action? ReconnectRequested;
    public event Action? OpenLogsRequested;

    public SettingsView()
    {
        InitializeComponent();
    }

    /// <summary>Renders a diagnostics report.</summary>
    public void Show(ShellDiagnostics? diagnostics)
    {
        if (diagnostics is null)
        {
            return;
        }

        ShellVersionText.Text = Or(diagnostics.ShellVersion);
        AbiText.Text = diagnostics.AbiVersion.ToString();
        DshPathText.Text = Or(diagnostics.DshPath);
        DshVersionText.Text = diagnostics.DshVersion is null
            ? "—"
            : diagnostics.DshVersionSupported == false
                ? $"{diagnostics.DshVersion}（不受支持）"
                : diagnostics.DshVersion;
        NodeVersionText.Text = Or(diagnostics.NodeVersion);
        ProfileText.Text = Or(diagnostics.Profile);
        PortText.Text = diagnostics.ConfiguredPort.ToString();
        LogDirText.Text = Or(diagnostics.LogDir);
        ConfigPathText.Text = Or(diagnostics.ConfigPath);
    }

    private static string Or(string? value) => string.IsNullOrWhiteSpace(value) ? "—" : value;

    private void OnBack(object sender, RoutedEventArgs e) => BackRequested?.Invoke();

    private void OnReconnect(object sender, RoutedEventArgs e) => ReconnectRequested?.Invoke();

    private void OnOpenLogs(object sender, RoutedEventArgs e) => OpenLogsRequested?.Invoke();
}
