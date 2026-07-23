# License

License is any of these or contact us for custom license options.

* [MIT](https://opensource.org/license/mit) ([SPDX: MIT](https://spdx.org/licenses/MIT.html))

* [Apache License 2.0](https://www.apache.org/licenses/LICENSE-2.0) ([SPDX: Apache-2.0](https://spdx.org/licenses/Apache-2.0.html))

* [GNU General Public License v2.0](https://www.gnu.org/licenses/old-licenses/gpl-2.0-standalone.html) ([SPDX: GPL-2.0-only](https://spdx.org/licenses/GPL-2.0-only.html))

Copyright © 2026 Joel Parker Henderson

## Notice — derivative work

`fhirpg` is a Rust translation of [fhirbase](https://github.com/fhirbase/fhirbase),
a Go command-line utility by the [Health Samurai](https://www.health-samurai.io/)
team. fhirbase is released under the MIT License:

> Copyright © 2018 Health Samurai
>
> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in all
> copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.

The MIT terms above apply to material derived from fhirbase, which includes:

* The SQL schema and stored-procedure assets under `assets/schema/`, vendored
  from fhirbase (renamed, and with identifiers rebranded per decision D3).
* The FHIR transformation-map assets under `assets/transform/`, vendored
  byte-identical from fhirbase.
* The web console assets under `assets/web/`, adapted from fhirbase.
* Rust source translated from fhirbase's Go source, which is most of `src/`.

The multi-license offer at the top of this file applies to original work in this
repository. It does not, and cannot, relicense the upstream material away from
its MIT terms — the notice above must travel with any copy.

## Notice — trademarks

FHIR® is a registered trademark of [Health Level Seven
International](https://www.hl7.org/). PostgreSQL® is a registered trademark of
the PostgreSQL Community Association of Canada. This project is not affiliated
with, endorsed by, or sponsored by either organization.
