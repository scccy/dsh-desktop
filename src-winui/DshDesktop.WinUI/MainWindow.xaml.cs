using DshDesktop.WinUI.Native;
using DshDesktop.WinUI.Views;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Windows.Graphics;

namespace DshDesktop.WinUI;

/// <summary>
/// The single shell window.
///
/// It has exactly three states, the same three the Tauri shell has: loading,
/// the hosted <c>dsh web</c> page, and a failure screen. All of the logic behind
/// those transitions lives in the Rust core; this class only maps core events to
/// XAML.
/// </summary>
public sealed partial class MainWindow : Window
{
    private readonly DshBridge _bridge;

    private LoadingView? _loading;
    private ErrorView? _error;
    private SettingsView? _settings;
    private Microsoft.UI.Xaml.Controls.WebView2? _browser;
    private UIElement? _beforeSettings;

    private string _lastUrl = string.Empty;
    private ShellDiagnostics _diagnostics = new();

    public MainWindow()
    {
        InitializeComponent();

        Title = "dsh desktop";
        SystemBackdrop = new MicaBackdrop();
        AppWindow.Resize(new SizeInt32(1280, 880));

        // The window itself exposes a DispatcherQueue property, so the static
        // factory has to be qualified by its type name.
        _bridge = new DshBridge(Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread());
        _bridge.StateChanged += OnStateChanged;

        RootGrid.Loaded += OnLoaded;
        Closed += OnClosed;
    }

    private async void OnLoaded(object sender, RoutedEventArgs e)
    {
        ShowLoading();

        try
        {
            _bridge.Connect();
        }
        catch (Exception error)
        {
            ShowError("internal", error.Message);
            return;
        }

        // Probing runs `dsh --version` and `node --version`, so it must not sit
        // on the UI thread.
        _diagnostics = await Task.Run(_bridge.Diagnose);

        if (!_diagnostics.DshFound)
        {
            ShowError("dsh-not-found", _diagnostics.Error);
            return;
        }

        StartConnection();
    }

    private void StartConnection()
    {
        ShowLoading();
        var rejection = _bridge.Start(new StartOptions());
        if (rejection is not null)
        {
            ShowError("internal", rejection);
        }
    }

    private void OnStateChanged(ShellState state)
    {
        switch (state.State)
        {
            case "connecting":
                ShowLoading();
                _loading?.SetStatus(state.Detail ?? "正在启动 dsh…");
                break;

            case "ready" when state.Url is not null:
                ShowBrowser(state.Url);
                break;

            case "failed":
                ShowError(state.Error ?? "internal", state.Detail);
                break;

            case "stopped":
                ShowLoading();
                break;
        }
    }

    private void ShowLoading()
    {
        if (_loading is null)
        {
            _loading = new LoadingView();
            _loading.OpenLogsRequested += () => _bridge.OpenLogs();
        }

        _beforeSettings = null;
        ScreenHost.Content = _loading;
    }

    private void ShowError(string kind, string? detail)
    {
        if (_error is null)
        {
            _error = new ErrorView();
            _error.RetryRequested += StartConnection;
            _error.SettingsRequested += ShowSettings;
            _error.OpenLogsRequested += () => _bridge.OpenLogs();
        }

        _error.Show(kind, detail);
        _beforeSettings = null;
        ScreenHost.Content = _error;
    }

    private void ShowBrowser(string url)
    {
        if (_browser is null)
        {
            _browser = new Microsoft.UI.Xaml.Controls.WebView2();
        }

        if (!string.Equals(url, _lastUrl, StringComparison.Ordinal))
        {
            // The first navigation carries the one-shot token; WebView2 follows
            // the server's redirect to the clean URL on its own.
            _browser.Source = new Uri(url);
            _lastUrl = url;
        }

        _beforeSettings = null;
        ScreenHost.Content = _browser;
    }

    private async void ShowSettings()
    {
        if (_settings is null)
        {
            _settings = new SettingsView();
            _settings.BackRequested += ReturnFromSettings;
            _settings.OpenLogsRequested += () => _bridge.OpenLogs();
            _settings.ReconnectRequested += () =>
            {
                ReturnFromSettings();
                StartConnection();
            };
        }

        _beforeSettings = ScreenHost.Content as UIElement;
        ScreenHost.Content = _settings;
        _settings.Show(_diagnostics);

        // Refresh in the background so a slow probe cannot freeze the page.
        _diagnostics = await Task.Run(_bridge.Diagnose);
        if (ReferenceEquals(ScreenHost.Content, _settings))
        {
            _settings.Show(_diagnostics);
        }
    }

    private void ReturnFromSettings()
    {
        ScreenHost.Content = _beforeSettings ?? (UIElement?)_loading;
    }

    private void OnClosed(object sender, WindowEventArgs args)
    {
        _bridge.Stop();
        _bridge.Dispose();
    }
}
