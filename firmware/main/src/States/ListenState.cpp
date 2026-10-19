#include "ListenState.h"
#include "BBStateMachine.h"
#include "Audio/AudioFrontend.h"
#include "Audio/SpeechAudioGen.h"
#include "Audio/SpeechAudioSource.h"
#include "Audio/SpeechGen.h"
#include <Core/Application.h>
#include <Statics/ApplicationStatics.h>
#include <Phrases.h>
#include <Services/Audio/Audio.h>
#include <Services/Audio/AACAudioGenerator.h>
#include "Audio/MemoryAudioSource.h"
#include "Util/PSRAMAllocator.h"
#include <algorithm>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <limits>
#include <memory>
#include <span>
#include <vector>
#include <Util/ServiceLocator.h>
#include "BBData.h"
#include "Enums.h"
#include "IdleState.h"
#include "RCState.h"
#include "ScenarioState.h"
#include "Services/Com.h"
#include "Services/ScenarioRoutineService.h"
#include "Services/IRStorage.h"
#include "Routines/IRActionRoutine.h"
#include "ScenarioRoutineMappings.h"

DEFINE_LOG(ListenState)

static void playPhraseBlocking(const Application* app, Phrase phrase){
    if(app == nullptr){
        return;
    }

    Audio* audio = app->getService<Audio>();
    if(audio == nullptr || !ServiceLocator::SpeechAudioGenInstance || !ServiceLocator::SpeechGenInstance){
        return;
    }

    const int16_t id = Phrases::get(phrase);
    if(id < 0){
        return;
    }

    const std::string text = Phrases::map(phrase, id);
    if(text.empty()){
        return;
    }

    auto source = std::make_unique<SpeechAudioSource>(SpeechGen::InputType::Text, text);
    audio->play(ServiceLocator::SpeechAudioGenInstance.get(), std::move(source));
    audio->waitEnd(portMAX_DELAY);
}

static const char* TAG = "ListenState";

namespace {
    constexpr uint32_t IntroWindowMs = 2500;

    struct SoundEffect { const char* path; uint16_t durationMs; };
    struct StartPrompt { const char* text; uint16_t durationMs; };

    constexpr StartPrompt StartPrompts[] = {
        { "Yes?",                  753 },
        { "I'm listening",         978 },
        { "How can I help?",      1303 },
        { "Another task?",        1010 },
        { "How may I assist?",    1417 },
        { "You have my attention", 1531 },
        { "Awaiting input",       1144 },
        { "State your request",   1511 },
    };

    constexpr SoundEffect Lasers1[] = {
        { "/spiffs/listen/lasers1/phaseJump1.aac",  384 },
        { "/spiffs/listen/lasers1/phaseJump2.aac",  384 },
        { "/spiffs/listen/lasers1/phaseJump3.aac",  320 },
        { "/spiffs/listen/lasers1/phaseJump4.aac",  256 },
        { "/spiffs/listen/lasers1/phaseJump5.aac",  384 },
        { "/spiffs/listen/lasers1/phaserDown1.aac", 384 },
        { "/spiffs/listen/lasers1/phaserDown2.aac", 320 },
        { "/spiffs/listen/lasers1/phaserDown3.aac", 384 },
    };

    constexpr SoundEffect Lasers2[] = {
        { "/spiffs/listen/lasers2/laser1.aac", 384 },
        { "/spiffs/listen/lasers2/laser2.aac", 384 },
        { "/spiffs/listen/lasers2/laser3.aac", 384 },
        { "/spiffs/listen/lasers2/laser4.aac", 384 },
        { "/spiffs/listen/lasers2/laser5.aac", 384 },
        { "/spiffs/listen/lasers2/laser6.aac", 320 },
        { "/spiffs/listen/lasers2/laser7.aac", 384 },
        { "/spiffs/listen/lasers2/laser8.aac", 384 },
        { "/spiffs/listen/lasers2/laser9.aac", 384 },
    };

    constexpr SoundEffect Robot[] = {
        { "/spiffs/listen/robot/robot_1.aac", 256 },
        { "/spiffs/listen/robot/robot_2.aac", 384 },
        { "/spiffs/listen/robot/robot_3.aac", 320 },
        { "/spiffs/listen/robot/robot_4.aac", 256 },
    };

    struct SfxGroup { const char* name; std::span<const SoundEffect> clips; };
    constexpr SfxGroup SfxGroups[] = {
        { "lasers1", Lasers1 },
        { "lasers2", Lasers2 },
        { "robot",   Robot   },
    };

    PSRAMByteBuffer readFile(const char* path){
        std::ifstream f(path, std::ios::binary | std::ios::ate);
        if(!f){
            return {};
        }
        const std::streamsize size = f.tellg();
        if(size <= 0){
            return {};
        }
        f.seekg(0);
        PSRAMByteBuffer buf(static_cast<size_t>(size));
        f.read(reinterpret_cast<char*>(buf.data()), size);
        buf.resize(static_cast<size_t>(f.gcount()));
        return buf;
    }

    void buildAndPlayIntro(const Application* app){
        Audio* audio = app->getService<Audio>();
        AACAudioGenerator* aacGen = app->getService<AACAudioGenerator>();
        if(audio == nullptr || aacGen == nullptr || ServiceLocator::SpeechAudioGenInstance == nullptr || ServiceLocator::SpeechGenInstance == nullptr){
            return;
        }

        const StartPrompt& prompt = StartPrompts[rand() % std::size(StartPrompts)];
        const uint32_t budget = prompt.durationMs >= IntroWindowMs ? 0 : IntroWindowMs - prompt.durationMs;

        const SfxGroup& group = SfxGroups[rand() % std::size(SfxGroups)];

        const uint32_t preloadStart = millis();

        const size_t maxAttempts = group.clips.size() * 4;

        std::vector<Audio::GenSourcePair> batch;
        batch.reserve(maxAttempts + 1); // + 1 for the spoken prompt source

        std::vector<size_t> pool;
        pool.reserve(group.clips.size());
        uint32_t accumulated = 0;
        size_t attempts = 0;
        while(accumulated < budget && attempts < maxAttempts){
            ++attempts;
            if(pool.empty()){
                for(size_t i = 0; i < group.clips.size(); ++i){
                    pool.push_back(i);
                }
            }
            const size_t poolIdx = rand() % pool.size();
            const SoundEffect& sfx = group.clips[pool[poolIdx]];
            pool.erase(pool.begin() + poolIdx);

            // Preload the clip into PSRAM now (before loadModel() hammers the flash),
            // so the audio task can play it without blocking on the flash lock.
            PSRAMByteBuffer bytes = readFile(sfx.path);
            if(bytes.empty()){
                continue;
            }

            batch.emplace_back(aacGen, std::make_unique<MemoryAudioSource>(std::move(bytes)));
            accumulated += sfx.durationMs;
        }

        batch.emplace_back(ServiceLocator::SpeechAudioGenInstance.get(), std::make_unique<SpeechAudioSource>(SpeechGen::InputType::Text, prompt.text));

        audio->play(std::span<Audio::GenSourcePair>(batch));
    }

}

std::vector<AudioFrontend::Phrase> ListenState::phrases;

void ListenState::preallocate(){
	phrases.reserve(activations.size() * 2);
}

ListenState::ListenState(BBStateMachine* sm) : State(sm){
	phrases.clear();

    const Application* app = ApplicationStatics::getApplication();

    buildAndPlayIntro(app);

    auto buttonInput = app->getService<ISRButtonInput>();
    if(buttonInput == nullptr){
        ESP_LOGE(TAG, "ButtonInput service missing!");
        return;
    }

    sm->bindState(buttonInput->OnButtonEvent, this, &ListenState::onButtonEvent);

    if(!ServiceLocator::ScenarioRoutineServiceInstance){
        CMF_LOG(ListenState, LogLevel::Error, "ScenarioRoutineService missing in constructor");
        return;
    }

    ServiceLocator::ScenarioRoutineServiceInstance->reset();

    com = ApplicationStatics::getApplication()->getService<Com>();
    if(com == nullptr){
        ESP_LOGE(TAG, "Com service missing!");
        return;
    }

    sm->bindState(com->OnCommand, this, &ListenState::onCommand);
    ListenData listenData{.phase = ListenData::Phase::Listening};
    com->sendData(BB::State::Listen, BB::Action::Listen{}, listenData);


    AudioFrontend* audioFrontend = app->getService<AudioFrontend>();
    if(audioFrontend == nullptr){
        CMF_LOG(ListenState, LogLevel::Error, "AudioFrontend service missing in constructor");
        return;
    }

	audioFrontend->loadModel();

    audioFrontend->setMode(AudioFrontend::Mode::Off);

    if(!activations.empty()){
        for(const ScenarioActivation& activation : activations){
            const RoutineFactory factory = routineForScenario({ activation.scenario, activation.scenarioData });
            phrases.push_back({ activation.string, activation.phonemes, activation.fuzzyCore, activation.fuzzyThreshold, reinterpret_cast<const void*>(factory) });
        }

        if(ServiceLocator::IRStorageInstance){
            for(uint8_t i = 0; i < ServiceLocator::IRStorageInstance->getCount(); ++i){
                const IRCommand& cmd = ServiceLocator::IRStorageInstance->getCommand(i);
                phrases.push_back({ cmd.phonemes, cmd.phonemes });
            }
        }

        audioFrontend->setSpeechPhrases(phrases);
    }

    if(Audio* audio = app->getService<Audio>()){
        audio->waitEnd(portMAX_DELAY);

        sm->bindState(audioFrontend->onPhrase, this, &ListenState::onPhrase);
        audioFrontend->setMode(AudioFrontend::Mode::Speech);
    }
}

ListenState::~ListenState(){
    // Event bindings are dropped by BBStateMachine::unbindStateEvents() after this dtor runs.
    const Application* app = ApplicationStatics::getApplication();
    if(AudioFrontend* audioFrontend = app->getService<AudioFrontend>()){
        audioFrontend->setMode(AudioFrontend::Mode::Off);
		audioFrontend->unloadModel();
    }

    if(timedOut){
        playPhraseBlocking(app, Phrase::ListeningTimeout);
    }
}

void ListenState::onRC(){
    sm->transitionTo<RCState>();
}

void ListenState::onPhrase(bool recognized, int index, const std::string& transcript, float confidence){
	// Runs on the AFE proc task. Do NOT stop the AFE or play audio here
    if(!recognized){
    	CMF_LOG(ListenState, LogLevel::Info, "Listen result: recognized=%d, index=%d, transcript='%s', confidence=%.2f",
			recognized, index, transcript.c_str(), confidence);

        ListenData listenData{.phase = ListenData::Phase::NotUnderstand};
        com->sendData(BB::State::Listen, BB::Action::Listen{}, listenData);
        timedOut = true;
        sm->transitionTo<IdleState>();
        return;
    }

	if(index < 0){
        // Abort phrase
        aborted();
        sm->transitionTo<IdleState>();
        return;
    }

    if(!ServiceLocator::ScenarioRoutineServiceInstance){
        CMF_LOG(ListenState, LogLevel::Error, "ScenarioRoutineService missing in onPhrase");
        return;
    }

    if(index >= static_cast<int>(activations.size())){
        const int irIndex = index - static_cast<int>(activations.size());
        if(ServiceLocator::IRStorageInstance){
            ServiceLocator::IRStorageInstance->setSelectedIndex(static_cast<uint8_t>(irIndex));
        }
        ServiceLocator::ScenarioRoutineServiceInstance->setRoutineFactory(&makeRoutine<IRActionRoutine>);
        sm->transitionTo<ScenarioState>();
        return;
    }

    const ScenarioActivation& activation = activations[index];
    const RoutineFactory factory = routineForScenario({ activation.scenario, activation.scenarioData });
    if(factory == nullptr){
        CMF_LOG(ListenState, LogLevel::Error, "No routine mapped for scenario %d", static_cast<int>(activation.scenario));
        sm->transitionTo<IdleState>();
        return;
    }

    ServiceLocator::ScenarioRoutineServiceInstance->setRoutineFactory(factory);
    sm->transitionTo<ScenarioState>();
}

void ListenState::onButtonEvent(int button, ISRButtonInput::Action action){
    if(action != ISRButtonInput::Action::Press || button != (int)Button::Power){
        return;
    }
    aborted();
    sm->transitionTo<IdleState>();
}

void ListenState::onCommand(Ctrl::Command cmd) {
    if(cmd == Ctrl::Command::Listen || cmd == Ctrl::ShutUp){
        aborted();
        sm->transitionTo<IdleState>();
    	return;
    }
}

void ListenState::aborted(){
	if(ServiceLocator::ScenarioRoutineServiceInstance){
		if(Audio* audio = ApplicationStatics::getApplication()->getService<Audio>()){
			audio->stop();
			audio->play(ServiceLocator::SpeechAudioGenInstance.get(), std::make_unique<SpeechAudioSource>(SpeechGen::InputType::Text, Phrases::map(Phrase::ListenAbort, Phrases::get(Phrase::ListenAbort))));
		}
	}

    if(com == nullptr){
        return;
    }

    ListenData listenData{.phase = ListenData::Phase::Abort};
    com->sendData(BB::State::Listen, BB::Action::Listen{}, listenData);
}
