const CACHE='c0mpute-shell-v3';
const SHELLS=['/','/chat'];
const EXT=/\.(?:css|js|png|svg|webmanifest|woff2?)$/i;
self.addEventListener('install',event=>event.waitUntil((async()=>{
 const cache=await caches.open(CACHE);
 try{
  for(const shell of SHELLS){
  const response=await fetch(shell,{credentials:'omit'});
  if(response.ok&&!response.redirected){
   await cache.put(shell,response.clone());
   const html=await response.text();
   const urls=[...html.matchAll(/(?:src|href)=["']([^"']+)["']/gi)].map(m=>new URL(m[1],self.location.origin));
   const assets=[...new Set(urls.filter(u=>u.origin===self.location.origin&&EXT.test(u.pathname)&&!u.pathname.startsWith('/api/')).map(u=>u.href))];
   await Promise.allSettled(assets.map(async url=>{const r=await fetch(url,{credentials:'omit'});if(r.ok)await cache.put(url,r)}));
  }
  }
 }catch{}
 await self.skipWaiting();
})()));
self.addEventListener('activate',event=>event.waitUntil((async()=>{for(const key of await caches.keys())if(key!==CACHE)await caches.delete(key);await self.clients.claim()})()));
self.addEventListener('fetch',event=>{
 const request=event.request,url=new URL(request.url);
 if(request.method!=='GET'||url.origin!==self.location.origin||url.pathname.startsWith('/api/'))return;
 if(request.mode==='navigate'&&SHELLS.includes(url.pathname)){event.respondWith(fetch(request).catch(()=>caches.match(url.pathname)));return}
 if(EXT.test(url.pathname)){event.respondWith(fetch(new Request(request,{credentials:'omit'})).then(response=>{if(response.ok)caches.open(CACHE).then(cache=>cache.put(request,response.clone()));return response}).catch(()=>caches.match(request)))}
});
