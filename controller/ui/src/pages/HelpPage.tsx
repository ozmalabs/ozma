import { Link } from 'react-router-dom'

const sections = [
  {
    title: 'Set up a computer',
    body: 'Choose what you want to do, select the computers, and check whether a route is available. Save the setup if you want to reopen the same choices later.',
  },
  {
    title: 'Review access on the target',
    body: 'Request access from setup. On the target computer, open the Ozma tray menu and choose “Review access on this computer”. Check the requested abilities before approving or denying.',
  },
  {
    title: 'Start and finish',
    body: 'Return to setup, refresh the approval status, then start the approved session. End it from the setup page when finished. A new control session needs a new approval.',
  },
]

export default function HelpPage() {
  return (
    <div className="mx-auto max-w-4xl space-y-8 p-6 text-oz-text sm:p-8">
      <header className="space-y-3">
        <p className="text-xs font-medium uppercase tracking-wider text-oz-primary">Ozma alpha</p>
        <h1 className="text-3xl font-semibold">Help and setup guide</h1>
        <p className="max-w-3xl leading-relaxed text-oz-muted">
          Ozma’s setup flow helps you choose an outcome, review access, and start a session without first choosing transports or codecs.
        </p>
      </header>

      <section aria-labelledby="journey-heading" className="space-y-4">
        <h2 id="journey-heading" className="text-xl font-semibold">The supported setup journey</h2>
        <ol className="grid gap-4 md:grid-cols-3">
          {sections.map((section, index) => (
            <li key={section.title} className="rounded-xl border border-oz-border bg-oz-surface p-5">
              <p className="text-sm font-medium text-oz-primary">Step {index + 1}</p>
              <h3 className="mt-2 font-medium">{section.title}</h3>
              <p className="mt-2 text-sm leading-relaxed text-oz-muted">{section.body}</p>
            </li>
          ))}
        </ol>
      </section>

      <section aria-labelledby="detail-heading" className="rounded-xl border border-oz-border bg-oz-surface p-5 sm:p-6">
        <h2 id="detail-heading" className="text-xl font-semibold">Choose how much detail to see</h2>
        <p className="mt-2 text-sm leading-relaxed text-oz-muted">
          Simple, Guided, Fine-tune, and Expert reveal progressively more configuration detail. Changing this aperture does not change your goal or grant access. Advanced image quality controls, including foveation, are unavailable until the selected runtime can apply them.
        </p>
      </section>

      <section aria-labelledby="limits-heading" className="rounded-xl border border-amber-400/30 bg-amber-400/5 p-5 sm:p-6">
        <h2 id="limits-heading" className="text-xl font-semibold">What this alpha does not confirm</h2>
        <ul className="mt-3 list-disc space-y-2 pl-5 text-sm leading-relaxed text-oz-muted">
          <li>A Ready session state confirms a lifecycle transition; it does not prove picture, sound, or keyboard/mouse input is connected.</li>
          <li>The tray’s controller and local-computer indicators check health endpoints only. They do not report active sessions or remote target reachability.</li>
          <li>Choose computers from setup. The tray does not switch the active target, pause a session, or supervise the controller and node.</li>
          <li>Quality, frame-rate, bandwidth, and foveation choices are not applied by this session path yet.</li>
        </ul>
      </section>

      <nav aria-label="Helpful pages" className="flex flex-wrap gap-3">
        <Link to="/setup" className="rounded-lg bg-oz-primary px-4 py-2.5 text-sm font-medium text-oz-bg hover:opacity-90">Open setup and choose computers</Link>
      </nav>
    </div>
  )
}
