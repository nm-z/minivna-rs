import java.io.BufferedReader;
import java.io.File;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;

import krause.vna.data.VNABaseSample;
import krause.vna.data.VNASampleBlock;
import krause.vna.data.VNAScanMode;
import krause.vna.data.calibrated.VNACalibratedSample;
import krause.vna.data.calibrated.VNACalibratedSampleBlock;
import krause.vna.data.calibrated.VNACalibrationBlock;
import krause.vna.data.calibrated.VNACalibrationContext;
import krause.vna.data.calibrated.VNACalibrationPoint;
import krause.vna.data.helper.VNACalibrationBlockHelper;
import krause.vna.device.serial.tiny.VNADriverSerialTiny;
import krause.vna.device.serial.tiny.VNADriverSerialTinyDIB;

/**
 * Development-only behavioral oracle for checking the Rust calibration math
 * against vna/J. Input rows are frequency_hz,real,imaginary.
 */
public final class LegacyCalibrationApply {
    private LegacyCalibrationApply() {
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 3 && args.length != 4) {
            System.err.println(
                    "usage: LegacyCalibrationApply CALIBRATION.cal RAW.csv TEMPERATURE_C [--trace]");
            System.exit(2);
        }
        boolean trace = args.length == 4 && "--trace".equals(args[3]);

        List<VNABaseSample> samples = new ArrayList<>();
        try (BufferedReader input =
                Files.newBufferedReader(Path.of(args[1]), StandardCharsets.UTF_8)) {
            String line;
            while ((line = input.readLine()) != null) {
                if (line.isBlank()) {
                    continue;
                }
                String[] fields = line.split(",");
                VNABaseSample sample = new VNABaseSample();
                sample.setFrequency(Long.parseLong(fields[0]));
                sample.setLoss(Double.parseDouble(fields[1]));
                sample.setAngle(Double.parseDouble(fields[2]));
                samples.add(sample);
            }
        }

        long startHz = samples.get(0).getFrequency();
        long stopHz = samples.get(samples.size() - 1).getFrequency();
        double temperatureC = Double.parseDouble(args[2]);

        VNADriverSerialTiny driver = new VNADriverSerialTiny();
        VNACalibrationBlock main =
                VNACalibrationBlockHelper.load(new File(args[0]), driver);
        VNACalibrationBlock resized =
                VNACalibrationBlockHelper.createResizedCalibrationBlock(
                        main, startHz, stopHz, samples.size());
        if (trace) {
            VNADriverSerialTinyDIB dib =
                    (VNADriverSerialTinyDIB) driver.getDeviceInfoBlock();
            System.err.printf(
                    "oracle calibration_temperature=%s temp_correction=%s "
                            + "gain_correction=%s phase_correction=%s "
                            + "if_phase_correction=%s%n",
                    hex(main.getTemperature()),
                    hex(dib.getTempCorrection()),
                    hex(dib.getGainCorrection()),
                    hex(dib.getPhaseCorrection()),
                    hex(dib.getIfPhaseCorrection()));
            int index = 0;
            for (VNACalibrationPoint point : resized.getCalibrationPoints()) {
                System.err.printf(
                        "oracle point=%d frequency=%d "
                                + "e00=(%s,%s) e11=(%s,%s) delta_e=(%s,%s)%n",
                        index++,
                        point.getFrequency(),
                        hex(point.getE00().getReal()),
                        hex(point.getE00().getImaginary()),
                        hex(point.getE11().getReal()),
                        hex(point.getE11().getImaginary()),
                        hex(point.getDeltaE().getReal()),
                        hex(point.getDeltaE().getImaginary()));
            }
        }

        VNASampleBlock raw = new VNASampleBlock();
        raw.setSamples(samples.toArray(VNABaseSample[]::new));
        raw.setNumberOfSteps(samples.size());
        raw.setStartFrequency(startHz);
        raw.setStopFrequency(stopHz);
        raw.setScanMode(VNAScanMode.MODE_REFLECTION);
        raw.setDeviceTemperature(temperatureC);

        VNACalibrationContext context =
                driver.getMathHelper().createCalibrationContextForCalibratedSamples(resized);
        VNACalibratedSampleBlock calibrated =
                driver.getMathHelper().createCalibratedSamples(context, raw);

        System.out.println(
                "frequency_hz,loss_db,phase_deg,resistance_ohm,swr,"
                        + "reactance_ohm,impedance_ohm,theta_deg");
        int sampleIndex = 0;
        for (VNACalibratedSample sample : calibrated.getCalibratedSamples()) {
            System.out.printf(
                    "%d,%.17g,%.17g,%.17g,%.17g,%.17g,%.17g,%.17g%n",
                    sample.getFrequency(),
                    sample.getReflectionLoss(),
                    sample.getReflectionPhase(),
                    sample.getR(),
                    sample.getSWR(),
                    sample.getX(),
                    sample.getZ(),
                    sample.getTheta());
            if (trace) {
                System.err.printf(
                        "oracle sample=%d frequency=%d "
                                + "loss=%s phase=%s r=%s swr=%s x=%s z=%s theta=%s%n",
                        sampleIndex,
                        sample.getFrequency(),
                        hex(sample.getReflectionLoss()),
                        hex(sample.getReflectionPhase()),
                        hex(sample.getR()),
                        hex(sample.getSWR()),
                        hex(sample.getX()),
                        hex(sample.getZ()),
                        hex(sample.getTheta()));
            }
            sampleIndex++;
        }
    }

    private static String hex(double value) {
        return Double.toHexString(value);
    }
}
