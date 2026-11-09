
#include "parameter.h"

#include <cstring>

namespace Clap::AUv2
{

Parameter::Parameter(const clap_plugin_t *plugin, const clap_plugin_params_t *clap_param_ext,
                     const clap_param_info_t &clap_param)
{
  updateInfo(plugin, clap_param_ext, clap_param);
}

void Parameter::updateInfo(const clap_plugin_t *plugin, const clap_plugin_params_t *clap_param_ext,
                           const clap_param_info_t &i)
{
  if (_cfstring)
  {
    CFRelease(_cfstring);
  }
  _info = i;
  _cfstring = CFStringCreateWithCString(NULL, _info.name, kCFStringEncodingUTF8);

  const auto &info = _info;
  AudioUnitParameterOptions flags = 0;

  flags |= kAudioUnitParameterFlag_Global;

  if (!(info.flags & CLAP_PARAM_IS_AUTOMATABLE)) flags |= kAudioUnitParameterFlag_NonRealTime;
  if (!(info.flags & CLAP_PARAM_IS_HIDDEN))
  {
    if (info.flags & CLAP_PARAM_IS_READONLY)
      flags |= kAudioUnitParameterFlag_IsReadable;
    else
      flags |= kAudioUnitParameterFlag_IsReadable | kAudioUnitParameterFlag_IsWritable;
  }
  // ConjureWidth patch (see deps/PATCHES.md): upstream added the unit to the flags, where those
  // numbers mean something else, so a host saw every control as continuous. Anything with two
  // steps isn't an on/off control: Mode's are Linked and Independent.
  _unit = kAudioUnitParameterUnit_Generic;
  if (info.flags & CLAP_PARAM_IS_STEPPED)
  {
    char first[200] = "", last[200] = "";
    clap_param_ext->value_to_text(plugin, info.id, info.min_value, first, sizeof(first));
    clap_param_ext->value_to_text(plugin, info.id, info.max_value, last, sizeof(last));
    if (info.max_value == 1 && info.min_value == 0 && !strcmp(first, "Off") && !strcmp(last, "On"))
      _unit = kAudioUnitParameterUnit_Boolean;
    else
      _unit = kAudioUnitParameterUnit_Indexed;
  }

  // we need this, otherwise hosts may quantize the parameter to 100 steps
  flags |= kAudioUnitParameterFlag_IsHighResolution;

  // checking if the parameter supports the conversion of its value to text

  // we can't get the value since we are not in the audio thread
  // auto guarantee_mainthread = _plugin->AlwaysMainThread();

  // ConjureWidth patch (see deps/PATCHES.md): an on/off control doesn't name its values, so the
  // host shows its own checkbox. Logic replaces the names of the second-to-last control with its
  // MIDI Mono Mode words, and an on/off control there is safe only without names.
  if (_unit != kAudioUnitParameterUnit_Boolean)
  {
    char buf[200];
    if (clap_param_ext->value_to_text(plugin, info.id, info.default_value, buf, sizeof(buf)))
    {
      flags |= kAudioUnitParameterFlag_HasName;
    }
  }

  /*
   * The CFString() used from the param can reset which releases it. So add a ref count
   * and ask the param to release it too
   */
  flags |= kAudioUnitParameterFlag_HasCFNameString | kAudioUnitParameterFlag_CFNameRelease;

  _flags = flags;
}
Parameter::~Parameter()
{
  CFRelease(_cfstring);
}

}  // namespace Clap::AUv2
