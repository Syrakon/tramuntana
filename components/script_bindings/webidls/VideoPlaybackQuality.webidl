/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/media-playback-quality/#videoplaybackquality

[Exposed=Window]
interface VideoPlaybackQuality {
  readonly attribute DOMHighResTimeStamp creationTime;
  readonly attribute unsigned long droppedVideoFrames;
  readonly attribute unsigned long totalVideoFrames;
  // Deprecated.
  readonly attribute unsigned long corruptedVideoFrames;
};
