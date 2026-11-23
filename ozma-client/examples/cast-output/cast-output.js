const castButton = document.querySelector("#cast");
const stopButton = document.querySelector("#stop");
const urlInput = document.querySelector("#url");
const status = document.querySelector("#status");

let context;
let starting = false;
let activeGrant = null;

window.__onGCastApiAvailable = available => {
  if (!available) {
    status.textContent = "Cast is unavailable in this browser.";
    return;
  }

  context = cast.framework.CastContext.getInstance();
  context.setOptions({
    receiverApplicationId: chrome.cast.media.DEFAULT_MEDIA_RECEIVER_APP_ID,
    autoJoinPolicy: chrome.cast.AutoJoinPolicy.ORIGIN_SCOPED,
  });
  context.addEventListener(cast.framework.CastContextEventType.SESSION_STATE_CHANGED, event => {
    if (event.sessionState === cast.framework.SessionState.SESSION_ENDED) {
      stopButton.disabled = true;
      status.textContent = "Casting stopped.";
    } else {
      stopButton.disabled = !context.getCurrentSession();
    }
  });
  castButton.disabled = false;
  status.textContent = "Choose a receiver to start casting.";
};

castButton.addEventListener("click", async () => {
  if (starting) return;
  starting = true;
  castButton.disabled = true;
  try {
    const url = new URL(urlInput.value);
    if (url.protocol !== "https:" || url.username || url.password) {
      throw new Error("Enter an HTTPS stream URL.");
    }
    if (!context.getCurrentSession()) await context.requestSession();
    const session = context.getCurrentSession();
    if (!session) throw new Error("No Cast receiver is connected.");

    const media = new chrome.cast.media.MediaInfo(url.href, "application/x-mpegurl");
    media.streamType = chrome.cast.media.StreamType.LIVE;
    const request = new chrome.cast.media.LoadRequest(media);
    request.autoplay = true;
    await session.loadMedia(request);
    activeGrant = /^\/cast\/[a-f0-9]{64}\/(?:live|master)\.m3u8$/.test(url.pathname)
      ? new URL(".", url).href.replace(/\/$/, "")
      : null;
    stopButton.disabled = false;
    status.textContent = "Casting Ozma output.";
  } catch {
    status.textContent = "Unable to start casting. Check the stream URL and receiver.";
  } finally {
    starting = false;
    castButton.disabled = false;
  }
});

stopButton.addEventListener("click", async () => {
  stopButton.disabled = true;
  urlInput.value = "";
  const grant = activeGrant;
  activeGrant = null;
  try {
    context.getCurrentSession()?.endSession(true);
  } catch {
    status.textContent = "Could not stop playback on the receiver.";
  }
  if (!grant) return;

  try {
    const response = await fetch(grant, {
      method: "DELETE",
      credentials: "omit",
      referrerPolicy: "no-referrer",
    });
    if (!response.ok && response.status !== 404) throw new Error("Revocation failed");
    status.textContent = "Cast playback grant revoked.";
  } catch {
    activeGrant = grant;
    stopButton.disabled = false;
    status.textContent = "Unable to revoke the playback grant. Try Stop again.";
  }
});
