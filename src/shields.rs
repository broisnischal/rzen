//! Built-in tracker/ad blocklist (used until the filter lists in `filters` are ready),
//! emitted in WebKit content-rule format, plus the YouTube ad script. Only third-party
//! loads are blocked, so visiting these sites directly still works.

const TRACKERS: &[&str] = &[
    "doubleclick.net",
    "googlesyndication.com",
    "googleadservices.com",
    "google-analytics.com",
    "googletagservices.com",
    "googletagmanager.com",
    "adservice.google.com",
    "amazon-adsystem.com",
    "adnxs.com",
    "adsrvr.org",
    "criteo.com",
    "criteo.net",
    "taboola.com",
    "outbrain.com",
    "scorecardresearch.com",
    "quantserve.com",
    "quantcount.com",
    "moatads.com",
    "rubiconproject.com",
    "pubmatic.com",
    "openx.net",
    "casalemedia.com",
    "indexww.com",
    "advertising.com",
    "bidswitch.net",
    "smartadserver.com",
    "yieldmo.com",
    "sharethrough.com",
    "teads.tv",
    "3lift.com",
    "media.net",
    "adform.net",
    "serving-sys.com",
    "mathtag.com",
    "contextweb.com",
    "lijit.com",
    "sovrn.com",
    "gumgum.com",
    "krxd.net",
    "bluekai.com",
    "demdex.net",
    "everesttech.net",
    "exelator.com",
    "rlcdn.com",
    "agkn.com",
    "tapad.com",
    "crwdcntrl.net",
    "adsafeprotected.com",
    "doubleverify.com",
    "hotjar.com",
    "clarity.ms",
    "bat.bing.com",
    "ads-twitter.com",
    "ads.linkedin.com",
    "mc.yandex.ru",
    "chartbeat.com",
    "chartbeat.net",
    "zemanta.com",
    "adroll.com",
    "33across.com",
    "spotxchange.com",
    "springserve.com",
    "zedo.com",
    "popads.net",
    "propellerads.com",
];

/// YouTube serves ads from its own servers, so request blocking can't stop them. This page
/// script (main world, document start, every frame) strips ad slots out of the player data
/// before the player reads it, skips any ad that still starts (muted, jumped to its end), and
/// hides sponsored shelves. Only runs on youtube.com; costs one MutationObserver on the player.
pub const YOUTUBE_JS: &str = r#"(()=>{
if(!/(^|\.)youtube(-nocookie)?\.com$/.test(location.hostname)||window.__rzenYt)return;
window.__rzenYt=1;
const KEYS=['adPlacements','adSlots','playerAds','adBreakHeartbeatParams'];
const strip=(o)=>{if(o&&typeof o==='object'){for(const k of KEYS)if(k in o)try{delete o[k]}catch(e){}const p=o.playerResponse;if(p&&typeof p==='object')for(const k of KEYS)if(k in p)try{delete p[k]}catch(e){}}return o};
JSON.parse=new Proxy(JSON.parse,{apply:(t,self,args)=>strip(Reflect.apply(t,self,args))});
const json=Response.prototype.json;
Response.prototype.json=new Proxy(json,{apply:(t,self,args)=>Reflect.apply(t,self,args).then(strip)});
let initial;
try{Object.defineProperty(window,'ytInitialPlayerResponse',{configurable:true,get:()=>initial,set:(v)=>{initial=strip(v)}})}catch(e){}
const css='ytd-ad-slot-renderer,ytd-in-feed-ad-layout-renderer,ytd-banner-promo-renderer,#player-ads,ytd-promoted-sparkles-web-renderer,.ytd-merch-shelf-renderer,ytd-rich-item-renderer:has(ytd-ad-slot-renderer),ytd-engagement-panel-section-list-renderer[target-id="engagement-panel-ads"]{display:none!important}';
const addCss=()=>{const s=document.createElement('style');s.textContent=css;(document.head||document.documentElement).appendChild(s)};
document.documentElement?addCss():document.addEventListener('DOMContentLoaded',addCss,{once:true});
let player=null,muted=false,timer=0;
const check=()=>{
if(!player)return;
const v=player.querySelector('video');
if(player.classList.contains('ad-showing')){
if(v){if(!v.muted){v.muted=true;muted=true}if(isFinite(v.duration)&&v.duration>0&&v.currentTime<v.duration-0.25)v.currentTime=v.duration}
for(const b of player.querySelectorAll('.ytp-skip-ad-button,.ytp-ad-skip-button,.ytp-ad-skip-button-modern'))b.click();
if(!timer)timer=setInterval(check,500);
}else{
if(timer){clearInterval(timer);timer=0}
if(muted&&v){v.muted=false;muted=false}
}
};
const mo=new MutationObserver(check);
const find=()=>{
const p=document.getElementById('movie_player');
if(p&&p!==player){mo.disconnect();player=p;mo.observe(p,{attributes:true,attributeFilter:['class']});check()}
if(!player||!player.isConnected){player=null;setTimeout(find,1000)}
};
document.readyState==='loading'?document.addEventListener('DOMContentLoaded',find,{once:true}):find();
document.addEventListener('yt-navigate-finish',find);
})();"#;

pub fn rules_json() -> String {
    let rules: Vec<String> = TRACKERS
        .iter()
        .map(|d| {
            let escaped = d.replace('.', r"\\.");
            format!(
                r#"{{"trigger":{{"url-filter":"^[^:]+://+([^:/]+\\.)?{escaped}[:/]","load-type":["third-party"]}},"action":{{"type":"block"}}}}"#
            )
        })
        .collect();
    format!("[{}]", rules.join(","))
}

#[cfg(test)]
mod tests {
    #[test]
    fn youtube_script_is_one_guarded_expression() {
        let js = super::YOUTUBE_JS;
        assert!(js.starts_with("(()=>{") && js.ends_with("})();"));
        assert!(js.contains("location.hostname") && js.contains("__rzenYt"));
    }

    #[test]
    fn rules_are_valid_json() {
        let v: serde_json::Value = serde_json::from_str(&super::rules_json()).unwrap();
        let first = v[0]["trigger"]["url-filter"].as_str().unwrap();
        assert_eq!(first, r"^[^:]+://+([^:/]+\.)?doubleclick\.net[:/]");
    }
}
