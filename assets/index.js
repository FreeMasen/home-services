Promise.sleep = ms => new Promise(r => setTimeout(r, ms));

(async () => {
    let sse = new EventSource("/sse");
    sse.onopen = e => {
        console.trace("sse:open");
        delete sse.onerror;
    };
    sse.onerror = e => {
        console.trace("sse:error");
        sse.close();
    }
    let to;
    sse.onmessage = ev => {
        delete sse.onerror;
        console.log("sse event", ev.data);
        clearTimeout(to);
        to = setTimeout(() => {
            console.log("debounced, reloading");
            sse.close();
            location = location;
        }, 200);
    }
    
})().catch(e => console.error("ERROR from main", e))
