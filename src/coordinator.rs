use crate::prelude::*;

// the coordinator takes messages from both MQ and the inverter and decides
// what to do with them.
//
// usually this will just be relaying directly out the other side, but some
// messages need a bit of state storing, eg "enable ac charge" is actually
// two inverter messages.

pub struct Coordinator {
    config: Rc<Config>,
    pub inverter: Inverter,
    pub mqtt: mqtt::Mqtt,
    from_inverter: lxp::inverter::PacketSender,
    to_inverter: lxp::inverter::PacketSender,
    from_mqtt: mqtt::MessageSender,
    to_mqtt: mqtt::MessageSender,
}

impl Coordinator {
    pub fn new(config: Rc<Config>) -> Self {
        let from_inverter = Self::channel();
        let to_inverter = Self::channel();
        let from_mqtt = Self::channel();
        let to_mqtt = Self::channel();

        // process messages from/to inverter, passing Packets
        let inverter = Inverter::new(
            Rc::clone(&config),
            to_inverter.clone(),
            from_inverter.clone(),
        );

        // process messages from/to MQTT, passing Messages
        let mqtt = mqtt::Mqtt::new(Rc::clone(&config), to_mqtt.clone(), from_mqtt.clone());

        Self {
            config,
            inverter,
            mqtt,
            from_inverter,
            to_inverter,
            from_mqtt,
            to_mqtt,
        }
    }

    pub async fn start(&self) -> Result<()> {
        loop {
            let f1 = self.inverter_receiver();
            let f2 = self.mqtt_receiver();

            let _ = futures::try_join!(f1, f2); // ignore result, just re-loop and restart
        }
    }

    async fn mqtt_receiver(&self) -> Result<()> {
        let mut receiver = self.from_mqtt.subscribe();

        loop {
            let message = receiver.recv().await?;
            //debug!("got message {:?}", message);

            let c = Command::try_from(message);
            match c {
                Ok(command) => {
                    debug!("parsed command {:?}", command);

                    let result = self.process_command(&command).await;

                    self.to_mqtt.send(mqtt::Message::command_result(
                        command.mqtt_topic(),
                        result.is_ok(),
                    ))?;

                    if let Err(err) = result {
                        error!("{:?}: {:?}", command, err);
                    }
                }
                Err(err) => {
                    // TODO need to send a FAIL here really
                    error!("{:?}", err);
                }
            }
        }
    }

    async fn process_command(&self, command: &Command) -> Result<()> {
        use lxp::packet::Register;
        use lxp::packet::RegisterBit;

        match *command {
            Command::ReadHold(register) => self.read_register(register).await,
            Command::AcCharge(enable) => {
                self.update_register(
                    Register::Register21.into(),
                    RegisterBit::AcChargeEnable,
                    enable,
                )
                .await
            }
            Command::ForcedDischarge(enable) => {
                self.update_register(
                    Register::Register21.into(),
                    RegisterBit::ForcedDischargeEnable,
                    enable,
                )
                .await
            }
            Command::ChargeRate(pct) => {
                self.set_register(Register::ChargePowerPercentCmd.into(), pct)
                    .await
            }
            Command::DischargeRate(pct) => {
                self.set_register(Register::DischgPowerPercentCmd.into(), pct)
                    .await
            }

            Command::AcChargeRate(pct) => {
                self.set_register(Register::AcChargePowerCmd.into(), pct)
                    .await
            }

            Command::AcChargeSocLimit(pct) => {
                self.set_register(Register::AcChargeSocLimit.into(), pct)
                    .await
            }

            Command::DischargeCutoffSocLimit(pct) => {
                self.set_register(Register::DischgCutOffSocEod.into(), pct)
                    .await
            }
        }
    }

    async fn read_register(&self, register: u16) -> Result<()> {
        let mut receiver = self.from_inverter.subscribe();

        let packet = self.make_packet(lxp::packet::DeviceFunction::ReadHold, register);
        self.to_inverter.send(Some(packet))?;

        Self::wait_for_packet(&mut receiver, register).await?;

        // note that we don't have to send an MQTT reply here.
        // inverter_receiver will do it for us!

        Ok(())
    }

    // TODO: could merge with update_register?
    async fn set_register(&self, register: u16, value: u16) -> Result<()> {
        let mut receiver = self.from_inverter.subscribe();

        let mut packet = self.make_packet(lxp::packet::DeviceFunction::WriteSingle, register);
        packet.set_value(value);
        self.to_inverter.send(Some(packet))?;

        let packet = Self::wait_for_packet(&mut receiver, register).await?;
        if packet.value() != value {
            return Err(anyhow!(
                "failed to set register {:?}, got back value {} (wanted {})",
                register,
                packet.value(),
                value
            ));
        }

        Ok(())
    }

    async fn update_register(
        &self,
        register: u16,
        bit: lxp::packet::RegisterBit,
        enable: bool,
    ) -> Result<()> {
        let mut receiver = self.from_inverter.subscribe();

        // get register from inverter
        let packet = self.make_packet(lxp::packet::DeviceFunction::ReadHold, register);
        self.to_inverter.send(Some(packet))?;

        let packet = Self::wait_for_packet(&mut receiver, register).await?;
        let value = if enable {
            packet.value() | u16::from(bit)
        } else {
            packet.value() & !u16::from(bit)
        };

        // new packet to set register with a new value
        let mut packet = self.make_packet(lxp::packet::DeviceFunction::WriteSingle, register);
        packet.set_value(value);
        self.to_inverter.send(Some(packet))?;

        let packet = Self::wait_for_packet(&mut receiver, register).await?;
        if packet.value() != value {
            return Err(anyhow!(
                "failed to update register {:?}, got back value {} (wanted {})",
                register,
                packet.value(),
                value
            ));
        }

        Ok(())
    }

    fn make_packet(&self, function: lxp::packet::DeviceFunction, register: u16) -> Packet {
        let mut packet = Packet::new();

        packet.set_tcp_function(lxp::packet::TcpFunction::TranslatedData);
        packet.set_device_function(function);
        packet.set_datalog(&self.config.inverter.datalog);
        packet.set_serial(&self.config.inverter.serial);
        packet.set_register(register);
        packet.set_value(1);

        packet
    }

    async fn wait_for_packet(
        receiver: &mut broadcast::Receiver<Option<Packet>>,
        register: u16,
    ) -> Result<Packet> {
        let start = std::time::Instant::now();

        loop {
            match receiver.try_recv() {
                Ok(Some(packet)) => {
                    if packet.register() == register {
                        return Ok(packet);
                    }
                }
                Ok(None) => return Err(anyhow!("inverter disconnect?")),
                Err(broadcast::error::TryRecvError::Empty) => {} // ignore and loop
                Err(err) => return Err(anyhow!("try_recv error: {:?}", err)),
            }

            if start.elapsed().as_secs() > 5 {
                return Err(anyhow!("wait_for_packet register={:?} - timeout", register));
            }

            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }

    async fn inverter_receiver(&self) -> Result<()> {
        let mut receiver = self.from_inverter.subscribe();

        // this loop holds no state so doesn't care about inverter reconnects
        loop {
            if let Some(packet) = receiver.recv().await? {
                // returns a Vec of messages to send. could be none;
                // not every packet needs an MQ message (eg, heartbeats)
                for message in mqtt::Message::from_packet(packet)? {
                    self.to_mqtt.send(message)?;
                }
            }
        }
    }

    fn channel<T: Clone>() -> broadcast::Sender<T> {
        let (tx, _) = broadcast::channel(16);
        tx
    }
}
