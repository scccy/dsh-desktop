using Microsoft.UI.Xaml;

namespace DshDesktop.WinUI;

/// <summary>
/// Application entry point. The window is the whole application: one window, one
/// instance, no documents — the same model the Tauri shell enforces.
/// </summary>
public partial class App : Application
{
    private Window? _window;
    private readonly bool _isSecondary;

    public App()
    {
        InitializeComponent();

        // A second launch must not start another dsh. The decision is made here
        // but acted on in OnLaunched: calling Environment.Exit while the
        // Application object is still being constructed tears XAML down and the
        // process dies with a stowed exception instead of exiting cleanly.
        _isSecondary = !SingleInstance.Acquire();

        // WebView2 defaults to a profile folder next to the executable, which
        // fails when the shell is installed somewhere read-only. The documented
        // environment variable moves it without touching the WinRT projection,
        // which trimming strips from the published build.
        Environment.SetEnvironmentVariable("WEBVIEW2_USER_DATA_FOLDER", ShellPaths.WebViewDataFolder());

        // A stowed XAML exception kills the process with 0xC000027B and no
        // message at all. The core's log lives in Rust, which never sees these,
        // so they are written next to it before the process goes down.
        UnhandledException += (_, args) =>
        {
            ErrorLog.Write("unhandled", args.Exception);
        };
    }

    protected override void OnLaunched(LaunchActivatedEventArgs args)
    {
        if (_isSecondary)
        {
            SingleInstance.SignalExisting();
            Exit();
            return;
        }

        try
        {
            var window = new MainWindow();
            _window = window;
            window.Activate();

            // A second launch brings this window forward instead of starting
            // a second shell (see SingleInstance).
            SingleInstance.Listen(() => window.DispatcherQueue.TryEnqueue(() =>
            {
                window.BringToFront();
                _window.Activate();
            }));
        }
        catch (Exception error)
        {
            ErrorLog.Write("launch", error);
            throw;
        }
    }
}

/// <summary>Per-user locations the shell writes to, independent of install path.</summary>
internal static class ShellPaths
{
    /// <summary>WebView2 profile directory; the app never writes beside its exe.</summary>
    public static string WebViewDataFolder()
    {
        var root = Path.Combine(
            Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
            "dsh desktop");
        Directory.CreateDirectory(root);
        return Path.Combine(root, "webview2");
    }
}

/// <summary>
/// Last-resort error log for failures that would otherwise kill the process
/// silently. Written beside the Rust core's logs so both sit in one folder.
/// </summary>
internal static class ErrorLog
{
    private static readonly object Gate = new();

    public static void Write(string source, Exception? error)
    {
        try
        {
            var folder = Path.Combine(
                Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
                "dsh desktop",
                "logs");
            Directory.CreateDirectory(folder);
            var line = $"{DateTimeOffset.UtcNow:O} [{source}] {error}\n\n";
            lock (Gate)
            {
                File.AppendAllText(Path.Combine(folder, "winui-crash.log"), line);
            }
        }
        catch
        {
            // Never let the logger itself take the process down.
        }
    }
}
