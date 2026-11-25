/* MagicMirror config fixture: comments, unquoted keys, single quotes, trailing commas. */
var config = {
  address: 'localhost',
  port: 8080,
  basePath: '/',
  language: 'en',
  units: 'metric',
  customCss: 'css/custom.css',
  logo: 'assets/logo.svg',
  modules: [
    {
      module: 'clock',
      position: 'top_left',
      config: { displaySeconds: true, },
    },
    {
      module: 'currentweather',
      position: 'top_right',
      config: {
        location: 'Testville',
        apiKey: 'redacted',
        weatherEndpoint: 'https://api.example.com/weather',
      },
    },
    { module: 'weatherforecast', position: 'top_right' },
    {
      module: 'calendar',
      position: 'top_left',
      config: {
        calendars: [
          { url: 'https://example.com/calendar.ics', symbol: 'calendar' },
        ],
      },
    },
    { module: 'newsfeed', position: 'bottom_bar' },
    { module: 'compliments', position: 'lower_third' },
    { module: 'alert', position: 'top_bar' },
    { module: 'updatenotification', position: 'top_bar' },
    {
      module: 'MMM-Remote-Control',
      position: 'bottom_right',
      config: {
        css: 'custom/remote.css',
        image: 'assets/remote.png',
      },
    },
    {
      module: 'totally-unknown',
      position: 'bottom_left',
      config: { foo: 'bar' },
    },
  ],
};

// This statement is deliberately executable. The importer must never run it.
require('fs').writeFileSync('/tmp/ozma_magicmirror_executed', 'executed');
