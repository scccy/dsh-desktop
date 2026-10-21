using System.Threading;

namespace DshDesktop.WinUI;

/// <summary>
/// One shell per user session.
///
/// The first instance holds a named mutex and listens on a named event; a second
/// launch signals that event and exits immediately — before it could start its own
/// <c>dsh</c>, which would fight for the port.
///
/// Names are prefixed with <c>Local\</c> so the scope is the logon session: two
/// different users may each run their own shell, which is what a per-user app
/// should do.
///
/// The listener is a dedicated thread on purpose (a thread-pool registration would
/// tie activation to the pool's availability), and `System.Threading.Thread` plus
/// `System.Threading.ThreadPool` are pinned in the project file so that trimming
/// cannot remove the assemblies this file needs — see `TrimmerRootAssembly` in
/// `DshDesktop.WinUI.csproj`.
/// </summary>
internal static class SingleInstance
{
    private const string MutexName = @"Local\dsh-desktop-single-instance";
    private const string EventName = @"Local\dsh-desktop-activate";

    // Held for the process lifetime: letting the mutex be collected would release
    // the lock and let a second instance in.
    private static Mutex? _mutex;
    private static EventWaitHandle? _activate;
    private static Thread? _listener;

    /// <summary>
    /// Takes the instance lock. Returns <c>false</c> when another shell already
    /// owns it, in which case the caller must signal it and exit.
    /// </summary>
    public static bool Acquire()
    {
        _mutex = new Mutex(initiallyOwned: true, MutexName, out var createdNew);
        if (!createdNew)
        {
            return false;
        }

        _activate = new EventWaitHandle(
            initialState: false,
            EventResetMode.AutoReset,
            EventName);
        return true;
    }

    /// <summary>Asks the running shell to come to the front. Best effort.</summary>
    public static void SignalExisting()
    {
        if (EventWaitHandle.TryOpenExisting(EventName, out var handle))
        {
            using (handle)
            {
                handle.Set();
            }
        }
    }

    /// <summary>
    /// Starts the background listener. <paramref name="onActivate"/> is invoked on
    /// the listener thread, so the callback must marshal to the UI thread itself.
    /// </summary>
    public static void Listen(Action onActivate)
    {
        if (_activate is null || _listener is not null)
        {
            return;
        }

        var signal = _activate;
        _listener = new Thread(() =>
        {
            while (true)
            {
                signal.WaitOne();
                try
                {
                    onActivate();
                }
                catch
                {
                    // An activation that fails must not take the shell down.
                }
            }
        })
        {
            IsBackground = true,
            Name = "single-instance",
        };
        _listener.Start();
    }
}
