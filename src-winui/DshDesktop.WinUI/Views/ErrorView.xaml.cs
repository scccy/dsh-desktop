using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;
using Windows.ApplicationModel.DataTransfer;

namespace DshDesktop.WinUI.Views;

/// <summary>
/// Failure screen. Every failure the shell can classify lands here with the
/// bottom-level text, a next step and an action — never a blank window.
/// </summary>
public sealed partial class ErrorView : UserControl
{
    public event Action? RetryRequested;
    public event Action? SettingsRequested;
    public event Action? OpenLogsRequested;

    public ErrorView()
    {
        InitializeComponent();
    }

    /// <summary>Fills the screen for one failure kind.</summary>
    public void Show(string kind, string? detail)
    {
        var content = ErrorCatalog.For(kind, detail);
        TitleText.Text = content.Title;
        BodyText.Text = content.Body;
        CommandText.Text = content.Command;
        StepList.ItemsSource = content.Steps;

        if (string.IsNullOrWhiteSpace(detail))
        {
            DetailText.Visibility = Visibility.Collapsed;
        }
        else
        {
            DetailText.Text = detail;
            DetailText.Visibility = Visibility.Visible;
        }
    }

    /// <summary>Small inline status under the buttons.</summary>
    public void Report(string text)
    {
        StatusText.Text = text;
    }

    private void OnRetry(object sender, RoutedEventArgs e)
    {
        Report("正在重试…");
        RetryRequested?.Invoke();
    }

    private void OnSettings(object sender, RoutedEventArgs e) => SettingsRequested?.Invoke();

    private void OnOpenLogs(object sender, RoutedEventArgs e) => OpenLogsRequested?.Invoke();

    private void OnCopyCommand(object sender, RoutedEventArgs e)
    {
        try
        {
            var package = new DataPackage();
            package.SetText(CommandText.Text);
            Clipboard.SetContent(package);
            Report("已复制到剪贴板");
        }
        catch (Exception)
        {
            Report("无法访问剪贴板");
        }
    }
}
