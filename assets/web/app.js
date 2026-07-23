window.onload = function() {
  var mime = "text/x-pgsql";

  const escapeHtml = unsafe => {
    return unsafe
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;")
      .replace(/'/g, "&#039;");
  };

  function tag(acc, tg, props) {
    acc.push("<" + tg);
    var cnt = Array.prototype.slice.call(arguments, 3);
    for (var k in props) {
      var v = props[k];
      acc.push(k + "=" + '"' + escapeHtml(v) + '"');
    }
    acc.push(">");
    Array.prototype.push.apply(acc, cnt);
    acc.push("</" + tg + ">");
  }

  const formatResultField = f => {
    if (typeof f === "object" && f !== null) {
      return "<pre>" + escapeHtml(JSON.stringify(f, null, 2)) + "</pre>";
    } else {
      return f;
    }
  };

  function runQuery(cm) {
    let q = cm.getValue();
    let url = new URL("/q", window.location);
    url.searchParams.append("query", q);

    document.getElementById("results").innerHTML =
      "<center>Loading...</center>";

    fetch(url)
      .then(response => {
        return response
          .json()
          .then(json => Promise.resolve([response.status, json]));
      })
      .then(resp => {
        const status = resp[0];
        const json = resp[1];

        if (status === 200) {
          console.log("Got results", json);

          let tbl =
            '<h3>Results</h3><table class="table table-striped table-bordered table-sm"><thead><tr>';

          json.columns.forEach(clmn => {
            tbl += "<th>" + clmn.Name + "</th>";
          });

          tbl += "</tr></thead><tbody>";

          json.rows.forEach(row => {
            tbl +=
              "<tr>" +
              row.map(f => "<td>" + formatResultField(f) + "</td>").join("") +
              "</tr>";
          });

          tbl += "</tbody></table>";

          document.getElementById("results").innerHTML = tbl;
        } else {
          document.getElementById("results").innerHTML =
            "<h3>Results</h3><div class='alert alert-danger'>" +
            json.message +
            "</div>";
        }
      })
      .catch(err => {
        document.getElementById("results").innerHTML =
          "<h3>Results</h3><div class='alert alert-danger'>" +
          err.message +
          "</div>";
      });
    return false;
  }

  window.submitQuery = () => {
    runQuery(window.editor);
  };

  window.editor = CodeMirror(document.getElementById("editor"), {
    mode: mime,
    theme: "duotone-light",
    indentWithTabs: true,
    smartIndent: true,
    lineNumbers: true,
    matchBrackets: true,
    value: "SELECT * FROM patient LIMIT 100;",
    autofocus: true,
    extraKeys: {
      "Ctrl-Space": "autocomplete",
      "Ctrl-Enter": runQuery
    },
    hintOptions: {
      tables: {
        users: ["name", "score", "birthDate"],
        countries: ["name", "population", "size"]
      }
    }
  });

  var data = {};
  window.doSelect = idx => {
    var item = data.queries[idx];
    item && item.query && window.editor.setValue(item.query);
  };

  window.doExec = idx => {
    var item = data.queries[idx];
    item && item.query && window.editor.setValue(item.query);
    runQuery(window.editor);
  };

  // Snippets are defined here rather than fetched from a third party.
  // fhirbase downloads them from fhirbase.github.io on every page load.
  data = {
    title: "Snippets",
    queries: [
      { title: "Count patients", query: "SELECT count(*) FROM patient;" },
      { title: "Rows per resource type",
        query: "SELECT relname AS resource_table, n_live_tup AS approx_rows\n  FROM pg_stat_user_tables\n WHERE n_live_tup > 0\n ORDER BY n_live_tup DESC;" },
      { title: "Patients by birth year",
        query: "SELECT left(resource->>'birthDate', 4) AS birth_year, count(*)\n  FROM patient\n GROUP BY 1 ORDER BY 1;" },
      { title: "Patients by gender",
        query: "SELECT resource->>'gender' AS gender, count(*)\n  FROM patient GROUP BY 1 ORDER BY 2 DESC;" },
      { title: "One patient, whole resource",
        query: "SELECT jsonb_pretty(resource) FROM patient LIMIT 1;" },
      { title: "Observations joined to their patient",
        query: "SELECT o.id, o.resource->'code'->>'text' AS code, p.resource->'name'->0->>'family' AS family\n  FROM observation o\n  JOIN patient p ON p.id = o.resource->'subject'->>'id'\n LIMIT 20;" },
      { title: "Read one resource through the API",
        query: "SELECT fhirpg_read('Patient', (SELECT id FROM patient LIMIT 1));" },
      { title: "Resource history",
        query: "SELECT id, txid, status, ts FROM patient_history ORDER BY ts DESC LIMIT 20;" }
    ]
  };

  (function renderSnippets() {
    var res = [];
    tag(res, "h3", {}, data.title);
    data.queries.forEach((x, i) => {
      tag(
        res,
        "a",
        {
          class: "item",
          href: "javascript:void(0)",
          title: x.query,
          onClick: "doSelect(" + i + ")",
          ondblclick: "doExec(" + i + ")"
        },
        x.title || x.query
      );
    });
    document.getElementById("right").innerHTML = res.join(" ");
  })();
};
