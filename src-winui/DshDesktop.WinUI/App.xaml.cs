using Microsoft.UI.Xaml;

namespace DshDesktop.WinUI;

/// <summary>
/// Application entry point. The window is the whole application: one window, one
/// instance, no documents — the same model the Tauri shell enforces.
/// </summary>
public partial class App : Application
{
    private Window? _window;

    public App()
    {
        InitializeComponent();
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        _window = new MainWindow();
        _window.Activate();
    }
}
