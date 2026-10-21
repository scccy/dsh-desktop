using Microsoft.UI.Xaml;
using Microsoft.UI.Xaml.Controls;

namespace DshDesktop.WinUI.Views;

/// <summary>First screen: the shell is starting dsh and has nothing to show yet.</summary>
public sealed partial class LoadingView : UserControl
{
    public event Action? OpenLogsRequested;

    public LoadingView()
    {
        InitializeComponent();
    }

    /// <summary>Replaces the status line; the spinner keeps running.</summary>
    public void SetStatus(string text)
    {
        if (!string.IsNullOrWhiteSpace(text))
        {
            StatusText.Text = text;
        }
    }

    private void OnOpenLogs(object sender, RoutedEventArgs e) => OpenLogsRequested?.Invoke();
}
