using DshDesktop.WinUI.Native;
using DshDesktop.WinUI.Views;
using Microsoft.UI;
using Microsoft.UI.Input;
using Microsoft.UI.Windowing;
using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Microsoft.UI.Xaml.Media;
using Windows.Foundation;
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

        ConfigureTitleBar();

        // The window itself exposes a DispatcherQueue property, so the static
        // factory has to be qualified by its type name.
        _bridge = new DshBridge(Microsoft.UI.Dispatching.DispatcherQueue.GetForCurrentThread());
        _bridge.StateChanged += OnStateChanged;

        RootGrid.Loaded += OnLoaded;
        Closed += OnClosed;
    }

    /// <summary>
    /// Takes over the caption row: the system title bar goes away, Windows keeps
    /// drawing the real caption buttons over the right edge, and the menus above
    /// stay clickable because only <see cref="DragRegion"/> is a drag surface.
    ///
    /// On Windows 10 builds without title-bar customization the extra row is
    /// removed rather than left dead, so those systems keep the system title bar.
    /// </summary>
    private void ConfigureTitleBar()
    {
        if (!AppWindowTitleBar.IsCustomizationSupported())
        {
            AppTitleBar.Visibility = Visibility.Collapsed;
            return;
        }

        var titleBar = AppWindow.TitleBar;
        titleBar.ExtendsContentIntoTitleBar = true;
        // WinUI only offers 32 or 48 DIP caption bands; the row below is 40, so
        // the caption buttons sit in a standard band and the row keeps its own
        // height. Sizes are re-read whenever the window changes DPI.
        titleBar.PreferredHeightOption = TitleBarHeightOption.Standard;
        titleBar.ButtonBackgroundColor = Colors.Transparent;
        titleBar.ButtonInactiveBackgroundColor = Colors.Transparent;

        SetTitleBar(DragRegion);
        UpdateTitleBarInsets();

        // Everything in the caption band is non-client by default, so the menus
        // took no mouse clicks at all while still accepting keyboard focus —
        // which looks like a dead menu. Declaring the interactive parts as
        // passthrough gives them real hit-testing.
        RootGrid.Loaded += (_, _) => UpdateTitleBarPassthrough();
        RootGrid.SizeChanged += (_, _) => UpdateTitleBarPassthrough();

        AppWindow.Changed += (_, args) =>
        {
            if (args.DidSizeChange || args.DidPresenterChange)
            {
                UpdateTitleBarInsets();
                UpdateTitleBarPassthrough();
            }
        };
    }

    /// <summary>
    /// Marks the brand block and the menu bar as client areas inside the caption
    /// band. Rectangles are physical pixels relative to the window's top-left, so
    /// they are rescaled with the window's DPI.
    /// </summary>
    private void UpdateTitleBarPassthrough()
    {
        if (AppTitleBar.Visibility != Visibility.Visible || RootGrid.XamlRoot is null)
        {
            return;
        }

        var scale = RootGrid.XamlRoot.RasterizationScale;
        var rects = new List<RectInt32>(2);
        foreach (var element in new FrameworkElement[] { BrandArea, AppMenu })
        {
            if (element.ActualWidth <= 0 || element.ActualHeight <= 0)
            {
                continue;
            }

            var bounds = element
                .TransformToVisual(null)
                .TransformBounds(new Rect(0, 0, element.ActualWidth, element.ActualHeight));
            rects.Add(new RectInt32(
                (int)Math.Round(bounds.X * scale),
                (int)Math.Round(bounds.Y * scale),
                (int)Math.Round(bounds.Width * scale),
                (int)Math.Round(bounds.Height * scale)));
        }

        InputNonClientPointerSource
            .GetForWindowId(AppWindow.Id)
            .SetRegionRects(NonClientRegionKind.Passthrough, rects.ToArray());
    }

    /// <summary>
    /// Keeps the menus clear of the caption buttons. The insets are reported by
    /// Windows and change with DPI and with the window's maximised state.
    /// </summary>
    private void UpdateTitleBarInsets()
    {
        var inset = AppWindow.TitleBar.RightInset;
        AppTitleBar.Padding = new Thickness(0, 0, Math.Max(0, inset), 0);
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

    // ---- caption row menus -------------------------------------------------
    // These are the shell's own actions. The hosted dsh page has its own menus
    // and nothing here reaches into it (no script is ever injected).

    private void OnMenuSettings(object sender, RoutedEventArgs e) => ShowSettings();

    private void OnMenuReconnect(object sender, RoutedEventArgs e) => StartConnection();

    private void OnMenuOpenLogs(object sender, RoutedEventArgs e) => _bridge.OpenLogs();

    private void OnMenuQuit(object sender, RoutedEventArgs e) => Close();

    private void OnMenuReload(object sender, RoutedEventArgs e)
    {
        // A plain reload of the page the shell already connected to; no new
        // navigation, so no new token is needed.
        if (_browser is not null && ReferenceEquals(ScreenHost.Content, _browser))
        {
            _browser.Reload();
        }
        else
        {
            StartConnection();
        }
    }

    private async void OnMenuAbout(object sender, RoutedEventArgs e)
    {
        var dsh = _diagnostics.DshVersion is null
            ? "未找到 dsh"
            : $"dsh {_diagnostics.DshVersion}";
        var dialog = new ContentDialog
        {
            XamlRoot = RootGrid.XamlRoot,
            Title = "dsh desktop",
            Content = string.Join(
                Environment.NewLine,
                $"壳版本 {_diagnostics.ShellVersion ?? "—"}",
                $"桥接 ABI {_diagnostics.AbiVersion}",
                dsh,
                _diagnostics.DshPath ?? "—"),
            CloseButtonText = "关闭",
        };
        await dialog.ShowAsync();
    }

    private void OnClosed(object sender, WindowEventArgs args)
    {
        _bridge.Stop();
        _bridge.Dispose();
    }
}
