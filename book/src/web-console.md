# The web console

```sh
fhirpg --db clinic web
```

```
SQL console on http://127.0.0.1:3000
Press Ctrl-C to stop.
```

A small browser UI: type SQL, press `Ctrl+Enter`, see results. Nested `jsonb`
comes back as JSON rather than as a quoted string, and there are a few built-in
snippets to start from.

## It executes arbitrary SQL with no authentication

That is the feature. Anyone who can reach the port can read, modify, or destroy
every resource in the database.

So it binds `127.0.0.1` by default, and exposing it takes a deliberate flag:

```sh
fhirpg --db clinic web --webhost 0.0.0.0
```

```
WARNING: the SQL console is bound to 0.0.0.0, not a loopback address.

The /q endpoint executes ARBITRARY SQL with NO AUTHENTICATION. Anyone
who can reach this port can read, modify, or destroy every resource in
the database. Do not do this on an untrusted network, and never against
a database holding real patient data.
```

fhirbase defaults `--webhost` to the empty string, which binds every interface.

## No tracking

fhirbase's console ships Google Analytics and Yandex Metrica — the latter with
session recording — on a page that renders patient query results, and reports
every SQL statement you run to Google as an analytics event. All of that is
removed here, along with the snippet list it fetched from a third party on every
page load. There is a test asserting the embedded assets contain none of it.

The page does still load Bootstrap, CodeMirror, and jQuery from public CDNs, so
it needs internet access to render. That is inherited and worth knowing about.

## Endpoints

| | |
| --- | --- |
| `GET /` | the console |
| `GET /q?query=…` | run SQL, return `{columns, rows}` |
| `GET /health` | `{"message":"ok"}` when the database answers |

A SQL error comes back as a non-200 with a message, never a crash — running
statements that turn out to be wrong is the normal case.

Values are rendered by PostgreSQL itself, so every type displays correctly,
including timestamps, arrays, enums, and types an extension added.
