/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://w3c.github.io/media-capabilities/

dictionary MediaConfiguration {
  VideoConfiguration video;
  AudioConfiguration audio;
};

dictionary MediaDecodingConfiguration : MediaConfiguration {
  required MediaDecodingType type;
  // MediaCapabilitiesKeySystemConfiguration keySystemConfiguration;
};

enum MediaDecodingType {
  "file",
  "media-source",
  "webrtc"
};

dictionary VideoConfiguration {
  required DOMString contentType;
  required unsigned long width;
  required unsigned long height;
  required unsigned long long bitrate;
  required double framerate;
  boolean hasAlphaChannel;
  HdrMetadataType hdrMetadataType;
  ColorGamut colorGamut;
  TransferFunction transferFunction;
  DOMString scalabilityMode;
  boolean spatialScalability;
};

enum HdrMetadataType {
  "smpteSt2086",
  "smpteSt2094-10",
  "smpteSt2094-40"
};

enum ColorGamut {
  "srgb",
  "p3",
  "rec2020"
};

enum TransferFunction {
  "srgb",
  "pq",
  "hlg"
};

dictionary AudioConfiguration {
  required DOMString contentType;
  DOMString channels;
  unsigned long long bitrate;
  unsigned long samplerate;
  boolean spatialRendering;
};

dictionary MediaCapabilitiesDecodingInfo {
  required boolean supported;
  required boolean smooth;
  required boolean powerEfficient;
  // required MediaKeySystemAccess? keySystemAccess;
  // MediaDecodingConfiguration configuration;
};

[Exposed=Window]
interface MediaCapabilities {
  [NewObject] Promise<MediaCapabilitiesDecodingInfo> decodingInfo(
      MediaDecodingConfiguration configuration);
  // [NewObject] Promise<MediaCapabilitiesEncodingInfo> encodingInfo(
  //     MediaEncodingConfiguration configuration);
};

partial interface Navigator {
  [SameObject] readonly attribute MediaCapabilities mediaCapabilities;
};
