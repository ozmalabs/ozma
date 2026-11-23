const castButton = document.querySelector("#cast");
const stopButton = document.querySelector("#stop");
const urlInput = document.querySelector("#url");
const status = document.querySelector("#status");

let context;
let starting = false;

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
    stopButton.disabled = false;
    status.textContent = "Casting Ozma output.";
  } catch {
    status.textContent = "Unable to start casting. Check the stream URL and receiver.";
  } finally {
    starting = false;
    castButton.disabled = false;
  }
});

stopButton.addEventListener("click", () => {
  context.getCurrentSession()?.endSession(true);
  stopButton.disabled = true;
  urlInput.value = "";
  status.textContent = "Stopping Cast playback.";
});
