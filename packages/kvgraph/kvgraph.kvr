fn new_graph(directed: Bool) -> Map {
    return {"directed": directed, "adj": {}, "nodes": {}};
}

fn add_node(g: Map, u: String) -> Map {
    let nodes_map = g["nodes"];
    let adj = g["adj"];
    if not contains(nodes_map, u) {
        nodes_map[u] = true;
    }
    if not contains(adj, u) {
        adj[u] = {};
    }
    g["nodes"] = nodes_map;
    g["adj"] = adj;
    return g;
}

fn add_edge(g: Map, u: String, v: String, weight: Number) -> Map {
    g = add_node(g, u);
    g = add_node(g, v);
    let adj = g["adj"];
    let u_edges = adj[u];
    u_edges[v] = weight;
    adj[u] = u_edges;
    if not g["directed"] {
        let v_edges = adj[v];
        v_edges[u] = weight;
        adj[v] = v_edges;
    }
    g["adj"] = adj;
    return g;
}

fn nodes(g: Map) -> List {
    return sorted(keys(g["nodes"]));
}

fn neighbors(g: Map, u: String) -> List {
    let adj = g["adj"];
    if not contains(adj, u) {
        return [];
    }
    return sorted(keys(adj[u]));
}

fn bfs(g: Map, start: String) -> List {
    if not contains(g["nodes"], start) {
        return [];
    }
    let visited = {};
    visited[start] = true;
    let queue = [start];
    let head = 0;
    let order = [];
    while head < len(queue) {
        let cur = queue[head];
        head = head + 1;
        order = append(order, cur);
        for nxt in neighbors(g, cur) {
            if not contains(visited, nxt) {
                visited[nxt] = true;
                queue = append(queue, nxt);
            }
        }
    }
    return order;
}

fn dfs(g: Map, start: String) -> List {
    if not contains(g["nodes"], start) {
        return [];
    }
    let visited = {};
    let order = [];
    let stack = [start];
    while len(stack) > 0 {
        let cur = stack[len(stack) - 1];
        stack = stack[0:(len(stack) - 1)];
        if contains(visited, cur) {
            continue;
        }
        visited[cur] = true;
        order = append(order, cur);
        let nbrs = neighbors(g, cur);
        let i = len(nbrs) - 1;
        while i >= 0 {
            let nxt = nbrs[i];
            if not contains(visited, nxt) {
                stack = append(stack, nxt);
            }
            i = i - 1;
        }
    }
    return order;
}

fn toposort(g: Map) -> List {
    if not g["directed"] {
        throw "toposort requires a directed graph";
    }
    let all_nodes = nodes(g);
    let indeg = {};
    for n in all_nodes {
        indeg[n] = 0;
    }
    for u in all_nodes {
        for v in neighbors(g, u) {
            indeg[v] = indeg[v] + 1;
        }
    }
    let ready = [];
    for n in all_nodes {
        if indeg[n] == 0 {
            ready = append(ready, n);
        }
    }
    let order = [];
    while len(ready) > 0 {
        ready = sorted(ready);
        let cur = ready[0];
        ready = ready[1:];
        order = append(order, cur);
        for nxt in neighbors(g, cur) {
            indeg[nxt] = indeg[nxt] - 1;
            if indeg[nxt] == 0 {
                ready = append(ready, nxt);
            }
        }
    }
    if len(order) != len(all_nodes) {
        throw "cycle detected in graph";
    }
    return order;
}

fn dijkstra(g: Map, start: String) -> Map {
    let dist = {};
    let prev = {};
    let visited = {};
    let all_nodes = nodes(g);
    if not contains(g["nodes"], start) {
        return {"dist": dist, "prev": prev};
    }
    dist[start] = 0.0;
    let adj = g["adj"];
    while true {
        let best = null;
        let best_d = 0.0;
        for n in all_nodes {
            if not contains(visited, n) and contains(dist, n) {
                if best == null or dist[n] < best_d {
                    best = n;
                    best_d = dist[n];
                }
            }
        }
        if best == null {
            break;
        }
        visited[best] = true;
        let edges = adj[best];
        for nxt in sorted(keys(edges)) {
            let w = edges[nxt];
            let cand = best_d + w;
            if (not contains(dist, nxt)) or cand < dist[nxt] {
                dist[nxt] = cand;
                prev[nxt] = best;
            }
        }
    }
    return {"dist": dist, "prev": prev};
}

fn shortest_path(g: Map, start: String, target: String) -> List {
    let res = dijkstra(g, start);
    let dist = res["dist"];
    if not contains(dist, target) {
        return [];
    }
    let prev = res["prev"];
    let rev = [target];
    let cur = target;
    while cur != start {
        if not contains(prev, cur) {
            return [];
        }
        cur = prev[cur];
        rev = append(rev, cur);
    }
    let out = [];
    let i = len(rev) - 1;
    while i >= 0 {
        out = append(out, rev[i]);
        i = i - 1;
    }
    return out;
}

fn connected_components(g: Map) -> List {
    let all_nodes = nodes(g);
    let seen = {};
    let comps = [];
    for n in all_nodes {
        if not contains(seen, n) {
            let comp = sorted(bfs(g, n));
            for c in comp {
                seen[c] = true;
            }
            comps = append(comps, comp);
        }
    }
    return comps;
}
